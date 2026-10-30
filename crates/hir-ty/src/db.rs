//! The home of `HirDatabase`, which is the Salsa database containing all the
//! type inference-related queries.

use std::sync::LazyLock;

use arrayvec::ArrayVec;
use base_db::{Crate, SourceDatabase, target::TargetLoadError};
use blaze_hashcons::Interned;
use either::Either;
use hir_def::{
    AdtId, BuiltinDeriveImplId, CallableDefId, ConstId, ConstParamId, EnumVariantId,
    ExpressionStoreOwnerId, FunctionId, GenericDefId, HasModule, ImplId, ModuleId,
    StaticId, TraitId, TypeAliasId, VariantId,
    builtin_derive::BuiltinDeriveImplMethod,
    expr_store::ExpressionStore,
    hir::{ClosureKind, ExprId},
    layout::TargetDataLayout,
    resolver::{HasResolver, Resolver},
    signatures::{ConstSignature, StaticSignature},
};
use salsa::SalsaValue;
use span::Edition;
use stdx::impl_from;

use crate::{
    GenericDefaultsRef, GenericPredicates, ImplTraitId, InferBodyId, TyDefId,
    TyLoweringDiagnostic, TyLoweringResult, ValueTyDefId,
    consteval::ConstEvalError,
    dyn_compatibility::DynCompatibilityViolation,
    layout::{InternedLayout, LayoutError},
    lower::{
        ConstParamTypes, FieldTypes, GenericDefaults, Lowered, SelfAndAssocBounds,
        TrackedStructToken, WithDefinedOpaques,
    },
    mir::{MirBody, MirEvalError, MirLowerError, MirOptions},
    next_solver::{
        Allocation, Clause, EarlyBinder, GenericArgs, ParamEnv, PolyFnSig, StoredEarlyBinder,
        StoredGenericArgs, StoredPolyFnSig, StoredTraitRef, StoredTy, TraitRef, Ty, VariancesOf,
    },
    traits::{ParamEnvAndCrate, StoredParamEnvAndCrate},
};

#[salsa::db]
pub trait HirDatabase: SourceDatabase + 'static {
    /// Manual implementation of upcasting from `dyn SourceDatabase` to `dyn HirDatabase`.
    ///
    /// This function is needed because Rust can't perform this upcasting automatically
    /// in the general case, as `Self` could be unsized.
    fn as_dyn(&self) -> &dyn HirDatabase;

    // region:mir

    // FIXME: Collapse `mir_body_for_closure` into `mir_body`
    // and `monomorphized_mir_body_for_closure` into `monomorphized_mir_body`
    fn mir_body<'db>(
        &'db self,
        def: InferBodyId<'db>,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn mir_body_for_closure<'db>(
        &'db self,
        def: InternedClosureId<'db>,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn monomorphized_mir_body<'db>(
        &'db self,
        def: InferBodyId<'db>,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn monomorphized_mir_body_for_closure<'db>(
        &'db self,
        def: InternedClosureId<'db>,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn mir_body_for_coroutine<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn mir_body_for_coroutine_by_move<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn monomorphized_mir_body_for_coroutine_by_move<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn monomorphized_mir_body_for_coroutine<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    /// The body as the interpreter and the code generator run it: after drop elaboration and
    /// the other passes to `MirPhase::Runtime`. The IDE asks for [`HirDatabase::mir_body`].
    fn runtime_mir_body<'db>(
        &'db self,
        def: InferBodyId<'db>,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn runtime_mir_body_for_closure<'db>(
        &'db self,
        def: InternedClosureId<'db>,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn runtime_mir_body_for_coroutine<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn runtime_mir_body_for_coroutine_by_move<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    /// The runtime body after rustc's MIR optimizations as `options` says
    /// ([`MirOptions`]): its level, its UB checks and whether lifetime markers are kept.
    fn optimized_mir_body<'db>(
        &'db self,
        def: InferBodyId<'db>,
        options: MirOptions,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn optimized_mir_body_for_closure<'db>(
        &'db self,
        def: InternedClosureId<'db>,
        options: MirOptions,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    /// The runtime body of a coroutine with the UB checks `options` asks for inserted; a
    /// coroutine is not optimized further.
    fn optimized_mir_body_for_coroutine<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
        options: MirOptions,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn optimized_mir_body_for_coroutine_by_move<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
        options: MirOptions,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn monomorphized_optimized_mir_body<'db>(
        &'db self,
        def: InferBodyId<'db>,
        options: MirOptions,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn monomorphized_optimized_mir_body_for_closure<'db>(
        &'db self,
        def: InternedClosureId<'db>,
        options: MirOptions,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn monomorphized_optimized_mir_body_for_coroutine<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
        options: MirOptions,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn monomorphized_optimized_mir_body_for_coroutine_by_move<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
        options: MirOptions,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn monomorphized_runtime_mir_body<'db>(
        &'db self,
        def: InferBodyId<'db>,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn monomorphized_runtime_mir_body_for_closure<'db>(
        &'db self,
        def: InternedClosureId<'db>,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn monomorphized_runtime_mir_body_for_coroutine<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn monomorphized_runtime_mir_body_for_coroutine_by_move<'db>(
        &'db self,
        def: InternedCoroutineId<'db>,
        subst: StoredGenericArgs,
        env: StoredParamEnvAndCrate,
    ) -> Result<&'db MirBody<'db>, MirLowerError<'db>>;

    fn const_eval<'db>(
        &'db self,
        def: ConstId,
        subst: GenericArgs<'db>,
        trait_env: Option<ParamEnvAndCrate<'db>>,
    ) -> Result<Allocation<'db>, ConstEvalError<'db>> {
        let db = self.as_dyn();
        crate::consteval::const_eval(db, def, subst, trait_env)
    }

    fn anon_const_eval<'db>(
        &'db self,
        def: AnonConstId<'db>,
        subst: GenericArgs<'db>,
        trait_env: Option<ParamEnvAndCrate<'db>>,
    ) -> Result<Allocation<'db>, ConstEvalError<'db>> {
        let db = self.as_dyn();
        crate::consteval::anon_const_eval(db, def, subst, trait_env)
    }

    fn const_eval_static<'db>(
        &'db self,
        def: StaticId,
    ) -> Result<Allocation<'db>, ConstEvalError<'db>> {
        let db = self.as_dyn();
        crate::consteval::const_eval_static(db, def)
    }

    fn const_eval_discriminant<'db>(
        &'db self,
        def: EnumVariantId,
    ) -> Result<i128, ConstEvalError<'db>> {
        let db = self.as_dyn();
        crate::consteval::const_eval_discriminant_variant(db, def)
    }

    fn lookup_impl_method<'db>(
        &'db self,
        env: ParamEnvAndCrate<'db>,
        func: FunctionId,
        fn_subst: GenericArgs<'db>,
    ) -> (Either<FunctionId, (BuiltinDeriveImplId, BuiltinDeriveImplMethod)>, GenericArgs<'db>)
    {
        let db = self.as_dyn();
        if !matches!(func.loc(db).container, hir_def::ItemContainerId::TraitId(_)) {
            return (Either::Left(func), fn_subst);
        }
        let (found, args) = crate::method_resolution::lookup_impl_method_stored(
            db,
            env.store(),
            func,
            fn_subst.store(),
        );
        (found, args.as_ref())
    }

    /// What the interpreter (`hir_ty_mir::interpret_mir`) gives running `body`: the value it
    /// returns.
    fn interpret_mir<'db>(
        &'db self,
        body: &'db MirBody<'db>,
        assert_placeholder_ty_is_unused: bool,
        trait_env: Option<ParamEnvAndCrate<'db>>,
    ) -> Result<Allocation<'db>, MirEvalError<'db>>;

    /// The opaque type of the impl's method that `opaque`, the `impl Trait` a trait's method
    /// returns, is with `args`, once they name the impl (`hir_ty_mir::opaque_of_impl`).
    fn opaque_of_impl<'db>(
        &'db self,
        env: ParamEnvAndCrate<'db>,
        opaque: InternedOpaqueTyId<'db>,
        args: GenericArgs<'db>,
    ) -> Option<Ty<'db>>;

    // endregion:mir

    fn layout_of_adt(
        &self,
        def: AdtId,
        args: StoredGenericArgs,
        trait_env: StoredParamEnvAndCrate,
    ) -> Result<Interned<'static, InternedLayout>, LayoutError> {
        let db = self.as_dyn();
        crate::layout::layout_of_adt_query(db, def, args, trait_env)
    }

    fn layout_of_ty(
        &self,
        ty: StoredTy,
        env: StoredParamEnvAndCrate,
    ) -> Result<Interned<'static, InternedLayout>, LayoutError> {
        let db = self.as_dyn();
        crate::layout::layout_of_ty_query(db, ty, env)
    }

    fn target_data_layout(&self, krate: Crate) -> Result<&TargetDataLayout, TargetLoadError> {
        let db = self.as_dyn();
        crate::layout::target_data_layout_query(db, krate).map_err(|err| err.clone())
    }

    fn target_data_layout_or_default(&self, krate: Crate) -> &TargetDataLayout {
        static DEFAULT: LazyLock<TargetDataLayout> = LazyLock::new(TargetDataLayout::default);

        let db = self.as_dyn();
        crate::layout::target_data_layout_query(db, krate).unwrap_or_else(|_| &*DEFAULT)
    }

    fn dyn_compatibility_of_trait(&self, trait_: TraitId) -> Option<DynCompatibilityViolation> {
        let db = self.as_dyn();
        crate::dyn_compatibility::dyn_compatibility_of_trait_query(db, trait_)
    }

    fn ty<'db>(&'db self, def: TyDefId) -> EarlyBinder<'db, Ty<'db>> {
        let db = self.as_dyn();
        crate::lower::ty_query(db, def)
    }

    fn type_for_type_alias_with_diagnostics<'db>(
        &'db self,
        def: TypeAliasId,
    ) -> &'db TyLoweringResult<'db, WithDefinedOpaques<StoredEarlyBinder<StoredTy>>> {
        let db = self.as_dyn();
        crate::lower::type_for_type_alias_with_diagnostics(db, def)
    }

    /// Returns the type of the value of the given constant, or `None` if the `ValueTyDefId` is
    /// a `StructId` or `EnumVariantId` with a record constructor.
    fn value_ty<'db>(&'db self, def: ValueTyDefId) -> Option<EarlyBinder<'db, Ty<'db>>> {
        let db = self.as_dyn();
        crate::lower::value_ty(db, def)
    }

    fn type_for_const<'db>(&'db self, def: ConstId) -> EarlyBinder<'db, Ty<'db>> {
        let db = self.as_dyn();
        crate::lower::type_for_const(db, def)
    }

    fn type_for_const_with_diagnostics<'db>(
        &'db self,
        def: ConstId,
    ) -> &'db TyLoweringResult<'db, StoredEarlyBinder<StoredTy>> {
        let db = self.as_dyn();
        crate::lower::type_for_const_with_diagnostics(db, def)
    }

    fn type_for_static<'db>(&'db self, def: StaticId) -> EarlyBinder<'db, Ty<'db>> {
        let db = self.as_dyn();
        crate::lower::type_for_static(db, def)
    }

    fn type_for_static_with_diagnostics<'db>(
        &'db self,
        def: StaticId,
    ) -> &'db TyLoweringResult<'db, StoredEarlyBinder<StoredTy>> {
        let db = self.as_dyn();
        crate::lower::type_for_static_with_diagnostics(db, def)
    }

    fn impl_self_ty_with_diagnostics<'db>(
        &'db self,
        def: ImplId,
    ) -> Lowered<'db, StoredEarlyBinder<StoredTy>> {
        let db = self.as_dyn();
        crate::lower::impl_self_ty_with_diagnostics(db, def)
    }

    fn impl_self_ty<'db>(&'db self, def: ImplId) -> EarlyBinder<'db, Ty<'db>> {
        let db = self.as_dyn();
        crate::lower::impl_self_ty_query(db, def)
    }

    fn const_param_types_with_diagnostics<'db>(
        &'db self,
        def: GenericDefId,
    ) -> &'db TyLoweringResult<'db, ConstParamTypes> {
        let db = self.as_dyn();
        crate::lower::const_param_types_with_diagnostics(db, def)
    }

    fn const_param_types(&self, def: GenericDefId) -> &ConstParamTypes {
        let db = self.as_dyn();
        crate::lower::const_param_types(db, def)
    }

    fn const_param_ty<'db>(&'db self, def: ConstParamId) -> Ty<'db> {
        let db = self.as_dyn();
        crate::lower::const_param_ty(db, def)
    }

    fn impl_trait_with_diagnostics<'db>(
        &'db self,
        def: ImplId,
    ) -> Option<Lowered<'db, StoredEarlyBinder<StoredTraitRef>>> {
        let db = self.as_dyn();
        crate::lower::impl_trait_with_diagnostics(db, def)
    }

    fn impl_trait<'db>(&'db self, def: ImplId) -> Option<EarlyBinder<'db, TraitRef<'db>>> {
        let db = self.as_dyn();
        crate::lower::impl_trait_query(db, def)
    }

    fn field_types_with_diagnostics<'db>(
        &'db self,
        var: VariantId,
    ) -> Lowered<'db, FieldTypes> {
        let db = self.as_dyn();
        crate::lower::field_types_with_diagnostics(db, var)
    }

    fn field_types(&self, var: VariantId) -> &FieldTypes {
        let db = self.as_dyn();
        crate::lower::field_types_query(db, var)
    }

    fn callable_item_signature<'db>(
        &'db self,
        def: CallableDefId,
    ) -> EarlyBinder<'db, PolyFnSig<'db>> {
        let db = self.as_dyn();
        crate::lower::callable_item_signature(db, def)
    }

    fn fn_sig_for_fn_with_diagnostics<'db>(
        &'db self,
        def: FunctionId,
    ) -> &'db TyLoweringResult<'db, WithDefinedOpaques<StoredEarlyBinder<StoredPolyFnSig>>> {
        let db = self.as_dyn();
        crate::lower::fn_sig_for_fn(db, def)
    }

    fn trait_environment<'db>(&'db self, def: GenericDefId) -> ParamEnv<'db> {
        let db = self.as_dyn();
        crate::lower::trait_environment(db, def)
    }

    fn generic_defaults_with_diagnostics<'db>(
        &'db self,
        def: GenericDefId,
    ) -> &'db TyLoweringResult<'db, GenericDefaults> {
        let db = self.as_dyn();
        crate::lower::generic_defaults_with_diagnostics(db, def)
    }

    /// This returns an empty list if no parameter has default.
    ///
    /// The binders of the returned defaults are only up to (not including) this parameter.
    fn generic_defaults(&self, def: GenericDefId) -> GenericDefaultsRef<'_> {
        let db = self.as_dyn();
        crate::lower::generic_defaults(db, def)
    }

    fn type_alias_bounds_with_diagnostics<'db>(
        &'db self,
        type_alias: TypeAliasId,
    ) -> &'db TyLoweringResult<'db, SelfAndAssocBounds> {
        let db = self.as_dyn();
        crate::lower::type_alias_bounds_with_diagnostics(db, type_alias)
    }

    fn type_alias_bounds<'db>(
        &'db self,
        type_alias: TypeAliasId,
    ) -> EarlyBinder<'db, &'db [Clause<'db>]> {
        let db = self.as_dyn();
        crate::lower::type_alias_bounds(db, type_alias)
    }

    fn type_alias_self_bounds<'db>(
        &'db self,
        type_alias: TypeAliasId,
    ) -> EarlyBinder<'db, &'db [Clause<'db>]> {
        let db = self.as_dyn();
        crate::lower::type_alias_self_bounds(db, type_alias)
    }

    fn variances_of<'db>(&'db self, def: GenericDefId) -> VariancesOf<'db> {
        let db = self.as_dyn();
        crate::variance::variances_of(db, def)
    }

    // HACK: We need this for MIR intrinsic `type_name()`, and it can't access the display infra because it is inside `hir-ide`.
    fn type_name<'db>(&'db self, ty: Ty<'db>, module: ModuleId) -> String;
}

#[test]
fn hir_database_is_dyn_compatible() {
    fn _assert_dyn_compatible(_: &dyn HirDatabase) {}
}

#[salsa::interned(debug, revisions = usize::MAX)]
#[derive(PartialOrd, Ord)]
pub struct InternedOpaqueTyId {
    #[returns(copy)]
    pub loc: ImplTraitId,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SalsaValue)]
pub struct InternedClosure<'db> {
    pub owner: InferBodyId<'db>,
    pub expr: ExprId,
    pub kind: ClosureKind,
}

/// What a closure is interned by: its place among the closures of its body. Its expression
/// moves with every edit of the body before it, and a closure that moved would be another
/// type, with everything generic over it instantiated again.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, SalsaValue)]
pub struct ClosureKey<'db> {
    owner: InferBodyId<'db>,
    nth: u32,
    kind: ClosureKind,
}

impl<'db> ClosureKey<'db> {
    fn new(db: &'db dyn HirDatabase, loc: InternedClosure<'db>) -> Self {
        let closures = closure_exprs(db, loc.owner.expression_store_owner(db), ());
        let nth = closures.partition_point(|&it| it < loc.expr) as u32;
        ClosureKey { owner: loc.owner, nth, kind: loc.kind }
    }

    /// Where the closure is among the closures of its body.
    pub fn nth(db: &'db dyn HirDatabase, loc: InternedClosure<'db>) -> u32 {
        Self::new(db, loc).nth
    }

    fn loc(self, db: &'db dyn HirDatabase) -> InternedClosure<'db> {
        let closures = closure_exprs(db, self.owner.expression_store_owner(db), ());
        // A key from before an edit that took closures out of the body names none of them.
        // Whatever asks about it is not asked for its answer.
        let expr = closures.get(self.nth as usize).or(closures.last()).copied();
        let expr = expr.unwrap_or_else(|| ExprId::from_raw(la_arena::RawIdx::from_u32(0)));
        InternedClosure { owner: self.owner, expr, kind: self.kind }
    }
}

/// The closures, `async` blocks and coroutines of a body, in the order of their expressions.
// The unit makes the arguments something salsa interns: the owner alone is no salsa struct.
#[salsa::tracked(returns(deref))]
fn closure_exprs(db: &dyn HirDatabase, owner: ExpressionStoreOwnerId, _: ()) -> Box<[ExprId]> {
    ExpressionStore::of(db, owner)
        .exprs()
        .filter(|(_, expr)| matches!(expr, hir_def::hir::Expr::Closure { .. }))
        .map(|(id, _)| id)
        .collect()
}

#[salsa::interned(constructor = new_impl, debug, revisions = usize::MAX)]
#[derive(PartialOrd, Ord)]
pub struct InternedClosureId<'db> {
    #[returns(copy)]
    key: ClosureKey<'db>,
}

impl<'db> InternedClosureId<'db> {
    pub fn loc(self, db: &'db dyn HirDatabase) -> InternedClosure<'db> {
        self.key(db).loc(db)
    }

    #[inline]
    pub fn new(db: &'db dyn HirDatabase, loc: InternedClosure<'db>) -> Self {
        if cfg!(debug_assertions) {
            let store = ExpressionStore::of(db, loc.owner.expression_store_owner(db));
            let expr = store.expr(loc.expr);
            assert!(
                matches!(
                    expr,
                    hir_def::hir::Expr::Closure {
                        closure_kind: hir_def::hir::ClosureKind::Closure
                            | hir_def::hir::ClosureKind::CoroutineClosure(_),
                        ..
                    }
                ),
                "expected a closure, found {expr:?}"
            );
        }

        Self::new_impl(db, ClosureKey::new(db, loc))
    }
}

#[salsa::interned(constructor = new_impl, debug, revisions = usize::MAX)]
#[derive(PartialOrd, Ord)]
pub struct InternedCoroutineId<'db> {
    #[returns(copy)]
    key: ClosureKey<'db>,
}

impl<'db> InternedCoroutineId<'db> {
    pub fn loc(self, db: &'db dyn HirDatabase) -> InternedClosure<'db> {
        self.key(db).loc(db)
    }

    #[inline]
    pub fn new(db: &'db dyn HirDatabase, loc: InternedClosure<'db>) -> Self {
        if cfg!(debug_assertions) {
            let store = ExpressionStore::of(db, loc.owner.expression_store_owner(db));
            let expr = store.expr(loc.expr);
            assert!(
                matches!(
                    expr,
                    hir_def::hir::Expr::Closure {
                        closure_kind: hir_def::hir::ClosureKind::OldCoroutine(_)
                            | hir_def::hir::ClosureKind::Coroutine { .. },
                        ..
                    }
                ),
                "expected a coroutine, found {expr:?}"
            );
        }

        Self::new_impl(db, ClosureKey::new(db, loc))
    }
}

#[salsa::interned(constructor = new_impl, debug, revisions = usize::MAX)]
#[derive(PartialOrd, Ord)]
pub struct InternedCoroutineClosureId<'db> {
    #[returns(copy)]
    key: ClosureKey<'db>,
}

impl<'db> InternedCoroutineClosureId<'db> {
    pub fn loc(self, db: &'db dyn HirDatabase) -> InternedClosure<'db> {
        self.key(db).loc(db)
    }

    #[inline]
    pub fn new(db: &'db dyn HirDatabase, loc: InternedClosure<'db>) -> Self {
        if cfg!(debug_assertions) {
            let store = ExpressionStore::of(db, loc.owner.expression_store_owner(db));
            let expr = store.expr(loc.expr);
            assert!(
                matches!(
                    expr,
                    hir_def::hir::Expr::Closure {
                        closure_kind: hir_def::hir::ClosureKind::CoroutineClosure(_),
                        ..
                    }
                ),
                "expected a coroutine closure, found {expr:?}"
            );
        }

        Self::new_impl(db, ClosureKey::new(db, loc))
    }

    /// The closure this is when called: its body returns the future of its `async` block.
    pub fn as_closure(self, db: &'db dyn HirDatabase) -> InternedClosureId<'db> {
        InternedClosureId::new(db, self.loc(db))
    }
}

/// An anonymous const expression that appears in a type position (e.g., array lengths,
/// const generic arguments like `{ N + 1 }`, or const param defaults). Unlike named constants,
/// these don't have their own `Body` — their expressions live in the parent's signature `ExpressionStore`.
#[derive(Debug, Hash, PartialEq, Eq, Clone)]
pub struct AnonConstLoc {
    /// The owner store containing this expression.
    pub owner: ExpressionStoreOwnerId,
    /// The ExprId within the owner's ExpressionStore that is the root
    /// of this anonymous const expression.
    pub expr: ExprId,
    pub ty: StoredEarlyBinder<StoredTy>,
    /// Whether to allow using generic params from the owner.
    /// true for array repeats, false for everything else.
    pub allow_using_generic_params: bool,
}

#[salsa::interned(debug, revisions = usize::MAX, constructor = new_)]
#[derive(PartialOrd, Ord)]
pub struct AnonConstId {
    #[returns(ref)]
    pub loc: AnonConstLoc,
}

impl<'db> AnonConstId<'db> {
    pub fn new(db: &'db dyn SourceDatabase, loc: AnonConstLoc, token: TrackedStructToken) -> Self {
        _ = token;
        AnonConstId::new_(db, loc)
    }
}

impl HasModule for AnonConstId<'_> {
    fn module(&self, db: &dyn SourceDatabase) -> ModuleId {
        self.loc(db).owner.module(db)
    }
}

impl HasResolver for AnonConstId<'_> {
    fn resolver(self, db: &dyn SourceDatabase) -> Resolver<'_> {
        self.loc(db).owner.resolver(db)
    }
}

pub fn signature_anon_consts_and_diagnostics<'db>(
    db: &'db dyn HirDatabase,
    def: GenericDefId,
) -> ArrayVec<(&'db [AnonConstId<'db>], &'db [TyLoweringDiagnostic]), 5> {
    let mut result = ArrayVec::new();

    // Queries common to all generic defs:
    push(&mut result, db.generic_defaults_with_diagnostics(def).as_lowered());
    push(&mut result, GenericPredicates::query_with_diagnostics(db, def));
    push(&mut result, db.const_param_types_with_diagnostics(def).as_lowered());

    match def {
        GenericDefId::ImplId(id) => {
            push(&mut result, db.impl_self_ty_with_diagnostics(id));
            if let Some(trait_ref) = db.impl_trait_with_diagnostics(id) {
                push(&mut result, trait_ref);
            }
        }
        GenericDefId::TypeAliasId(id) => {
            push(&mut result, db.type_for_type_alias_with_diagnostics(id).as_lowered());
            push(&mut result, db.type_alias_bounds_with_diagnostics(id).as_lowered());
        }
        GenericDefId::FunctionId(id) => {
            push(&mut result, db.fn_sig_for_fn_with_diagnostics(id).as_lowered())
        }
        GenericDefId::ConstId(def) => {
            push(&mut result, db.type_for_const_with_diagnostics(def).as_lowered())
        }
        GenericDefId::StaticId(def) => {
            push(&mut result, db.type_for_static_with_diagnostics(def).as_lowered())
        }
        GenericDefId::TraitId(_) | GenericDefId::AdtId(_) => {}
    }

    return result;

    fn push<'db, T: ?Sized>(
        result: &mut ArrayVec<(&'db [AnonConstId<'db>], &'db [TyLoweringDiagnostic]), 5>,
        item: crate::lower::Lowered<'db, T>,
    ) {
        result.push((item.defined_anon_consts(), item.diagnostics()));
    }
}

impl<'db> AnonConstId<'db> {
    /// A `const { .. }` block of a body, which MIR lowering makes a constant of its own. The
    /// body inferred it.
    pub fn is_inline_const(self, db: &'db dyn HirDatabase) -> bool {
        let loc = self.loc(db);
        matches!(loc.owner, ExpressionStoreOwnerId::Body(_))
            && matches!(
                hir_def::expr_store::ExpressionStore::of(db, loc.owner).expr(loc.expr),
                hir_def::hir::Expr::Const(_)
            )
    }

    pub fn all_from_signature(
        db: &'db dyn HirDatabase,
        def: GenericDefId,
    ) -> impl Iterator<Item = AnonConstId<'db>> {
        signature_anon_consts_and_diagnostics(db, def)
            .into_iter()
            .flat_map(|(anon_consts, _)| anon_consts)
            .copied()
    }
}

/// A constant, which might appears as a const item, an anonymous const block in expressions
/// or patterns, or as a constant in types with const generics.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, salsa::Supertype)]
pub enum GeneralConstId<'db> {
    ConstId(ConstId),
    StaticId(StaticId),
    AnonConstId(AnonConstId<'db>),
}

impl_from!(impl<'db> ConstId, StaticId, AnonConstId<'db> for GeneralConstId<'db>);

impl<'db> GeneralConstId<'db> {
    pub fn generic_def(self, db: &'db dyn HirDatabase) -> Option<GenericDefId> {
        match self {
            GeneralConstId::ConstId(it) => Some(it.into()),
            GeneralConstId::StaticId(it) => Some(it.into()),
            GeneralConstId::AnonConstId(it) => Some(it.loc(db).owner.generic_def(db)),
        }
    }

    pub fn name(self, db: &'db dyn SourceDatabase) -> String {
        match self {
            GeneralConstId::StaticId(it) => {
                StaticSignature::of(db, it).name.display(db, Edition::CURRENT).to_string()
            }
            GeneralConstId::ConstId(const_id) => {
                ConstSignature::of(db, const_id).name.as_ref().map_or_else(
                    || "_".to_owned(),
                    |name| name.display(db, Edition::CURRENT).to_string(),
                )
            }
            GeneralConstId::AnonConstId(_) => "{const}".to_owned(),
        }
    }
}

/// The kind numbers of hir-ty's queries and interned ids, for blaze-query and the
/// why-did-this-rerun explainer: hir-ty's part of blaze-query's `FRONT` range. A number is
/// never given to another query, nor reused once its query is gone.
pub mod kinds {
    pub use blaze_query::QueryKind;

    /// The numbers hir-ty may give.
    pub const RANGE: std::ops::Range<u16> = 0x2400..0x2600;

    macro_rules! kinds {
        ($($(#[$meta:meta])* $name:ident = $number:literal, $query:literal;)*) => {
            $($(#[$meta])* pub const $name: QueryKind = QueryKind($number);)*

            /// Every kind, with its query as hir-ty names it: a function's name, or
            /// `Type::method` for a method of a tracked `impl`.
            pub const ALL: &[(QueryKind, &str)] = &[$(($name, $query)),*];

            /// The kind of the query or interned id hir-ty names `name`, as [`ALL`] names it.
            pub fn of(name: &str) -> Option<QueryKind> {
                match name {
                    $($query => Some($name),)*
                    _ => None,
                }
            }
        };
    }

    /// Each number of [`RANGE`] with the query it is given to, built when compiled: a lookup
    /// is one bounds check and one load, 8 KiB. Building it refuses a number outside the range
    /// or given twice.
    const NAMES: [Option<&str>; (RANGE.end - RANGE.start) as usize] = {
        let mut names = [None; (RANGE.end - RANGE.start) as usize];
        let mut at = 0;
        while at < ALL.len() {
            let (kind, name) = ALL[at];
            assert!(RANGE.start <= kind.0 && kind.0 < RANGE.end, "a kind outside hir-ty's range");
            let slot = (kind.0 - RANGE.start) as usize;
            assert!(names[slot].is_none(), "a kind given to two queries");
            names[slot] = Some(name);
            at += 1;
        }
        names
    };

    /// The query or interned id of `kind`, if it is hir-ty's.
    pub fn name(kind: QueryKind) -> Option<&'static str> {
        NAMES.get(kind.0.wrapping_sub(RANGE.start) as usize).copied().flatten()
    }

    kinds! {
        // Bodies.
        INFER = 0x2400, "infer_body_query";
        INFER_ANON_CONST = 0x2401, "infer_anon_const_body_query";
        CLOSURE_EXPRS = 0x2402, "closure_exprs";

        // Const eval.
        CONST_EVAL = 0x2410, "const_eval_query";
        ANON_CONST_EVAL = 0x2411, "anon_const_eval_query";
        CONST_EVAL_STATIC = 0x2412, "const_eval_static_query";
        CONST_EVAL_DISCRIMINANT = 0x2413, "const_eval_discriminant_variant";

        // Signatures.
        IMPL_TRAIT = 0x2420, "impl_trait_of_impl";
        TYPE_FOR_CONST = 0x2421, "type_for_const_with_diagnostics";
        TYPE_FOR_STATIC = 0x2422, "type_for_static_with_diagnostics";
        TYPE_FOR_TYPE_ALIAS = 0x2423, "type_for_type_alias_with_diagnostics";
        IMPL_SELF_TY = 0x2424, "impl_self_ty_of_impl";
        CONST_PARAM_TYPES = 0x2425, "const_param_types_with_diagnostics";
        FIELD_TYPES = 0x2426, "field_types_of_variant";
        SUPERTRAITS_INFO = 0x2427, "supertraits_info";
        ASSOC_TYPE_SHORTHAND = 0x2428, "resolve_type_param_assoc_type_shorthand";
        TYPE_ALIAS_BOUNDS = 0x2429, "type_alias_bounds_with_diagnostics";
        GENERIC_PREDICATES = 0x242a, "generic_predicates_of";
        TRAIT_ENVIRONMENT = 0x242b, "trait_environment_query";
        CONST_CONDITIONS = 0x242c, "const_conditions_query";
        GENERIC_DEFAULTS = 0x242d, "generic_defaults_with_diagnostics";
        FN_SIG_FOR_FN = 0x242e, "fn_sig_for_fn";
        ELIDED_LIFETIMES = 0x242f, "elided_lifetimes";
        FN_SIG_FOR_STRUCT_CONSTRUCTOR = 0x2430, "fn_sig_for_struct_constructor";
        FN_SIG_FOR_ENUM_VARIANT_CONSTRUCTOR = 0x2431, "fn_sig_for_enum_variant_constructor";
        BUILTIN_DERIVE_PREDICATES = 0x2432, "predicates";
        CONST_PARAM_ENV = 0x2433, "const_param_env_query";
        ITEM_BOUNDS = 0x2434, "ItemBounds::of";
        IMPL_SUPER_OUTLIVES = 0x2435, "impl_super_outlives";
        CRATE_FIELD_TYPES = 0x2436, "crate_field_types";
        CRATE_IMPL_SELF_TYS = 0x2437, "crate_impl_self_tys";
        CRATE_IMPL_TRAITS = 0x2438, "crate_impl_traits";
        CRATE_IMPL_PREDICATES = 0x2439, "crate_impl_predicates";

        // Variance, dyn compatibility, representability.
        VARIANCES_OF = 0x2440, "variances_of_query";
        CTOR_VARIANCES = 0x2441, "ctor_variances_query";
        DYN_COMPATIBILITY_OF_TRAIT = 0x2442, "dyn_compatibility_of_trait_query";
        GENERICS_REQUIRE_SIZED_SELF = 0x2443, "generics_require_sized_self";
        REPRESENTABILITY = 0x2444, "representability";
        PARAMS_IN_REPR = 0x2445, "params_in_repr";

        // Layout and drop.
        LAYOUT_OF_ADT = 0x2450, "layout_of_adt_query";
        LAYOUT_OF_TY = 0x2451, "layout_of_ty_query";
        TARGET_DATA_LAYOUT = 0x2452, "target_data_layout_query";
        STRUCT_FLAGS = 0x2453, "struct_flags";
        VARIANT_FIELD_IDS = 0x2454, "variant_field_ids";
        ENUM_VARIANT_IDS = 0x2455, "enum_variant_ids";
        DESTRUCTOR = 0x2456, "destructor";
        STRUCT_TAIL_MAY_BE_UNSIZED = 0x2457, "struct_tail_may_be_unsized";

        // Method resolution, impls, coherence, specialization.
        LOOKUP_IMPL_METHOD = 0x2460, "lookup_impl_method_stored";
        IMPL_VALUE_ITEM = 0x2461, "impl_value_item";
        TRAIT_VALUE_ITEM = 0x2462, "trait_value_item";
        LOADED_CRATES = 0x2463, "loaded_crates";
        IMPLS_IN_BODIES = 0x2464, "impls_in_bodies";
        MACROS_WRITING_IMPLS = 0x2465, "macros_writing_impls";
        MACRO_DEFINITIONS = 0x2466, "macro_definitions";
        CRATES_WITH_INCOHERENT_IMPLS = 0x2467, "crates_containing_incoherent_inherent_impls";
        INHERENT_IMPLS_FOR_CRATE = 0x2468, "InherentImpls::for_crate";
        INHERENT_IMPLS_IN_CRATE_FOR_SELF_TY = 0x2469, "InherentImpls::in_crate_for_self_ty";
        INHERENT_IMPLS_FOR_BLOCK = 0x246a, "InherentImpls::for_block";
        INHERENT_IMPLS_IN_BLOCK_FOR_SELF_TY = 0x246b, "InherentImpls::in_block_for_self_ty";
        TRAIT_IMPLS_FOR_CRATE = 0x246c, "TraitImpls::for_crate";
        TRAIT_IMPLS_FOR_BLOCK = 0x246d, "TraitImpls::for_block";
        TRAIT_IMPLS_FOR_CRATE_AND_DEPS = 0x246e, "TraitImpls::for_crate_and_deps";
        TRAIT_IMPLS_IN_CRATE_AND_DEPS_FOR_TRAIT_AND_SELF_TY = 0x246f,
            "TraitImpls::in_crate_and_deps_for_trait_and_self_ty";
        TRAIT_IMPLS_BLANKET_IN_CRATE_AND_DEPS = 0x2470, "TraitImpls::blanket_in_crate_and_deps";
        TRAIT_IMPLS_IN_CRATE_FOR_TRAIT_AND_SELF_TY = 0x2471,
            "TraitImpls::in_crate_for_trait_and_self_ty";
        TRAIT_IMPLS_IN_BLOCK_FOR_TRAIT_AND_SELF_TY = 0x2472,
            "TraitImpls::in_block_for_trait_and_self_ty";
        TRAIT_IMPLS_IN_CRATE_FOR_TRAIT = 0x2473, "TraitImpls::in_crate_for_trait";
        TRAIT_IMPLS_IN_CRATE_AND_DEPS_FOR_TRAIT = 0x2474, "TraitImpls::in_crate_and_deps_for_trait";
        INHERENT_IMPL_COHERENT = 0x2475, "inherent_impl_coherent";
        ORPHAN_RULES_HOLD = 0x2476, "orphan_rules_hold";
        SPECIALIZES = 0x2477, "specializes_query";
        IMPL_HAS_UNCONSTRAINED_PARAMS = 0x2478, "impl_has_unconstrained_params";
        TRAIT_VALUE_ITEMS = 0x2479, "trait_value_items";
        IN_CRATE_TRAIT_VALUE_ITEMS = 0x247a, "in_crate_trait_value_items";

        // Opaque types.
        IMPL_TYPE_ALIASES = 0x2480, "impl_type_aliases";
        TRAIT_TYPE_ALIASES = 0x2481, "trait_type_aliases";
        RPIT_HIDDEN_TYPES = 0x2482, "rpit_hidden_types";
        TAIT_HIDDEN_TYPES = 0x2483, "tait_hidden_types";
        DEFINING_FUNCTIONS = 0x2484, "defining_functions";
        DEFINE_OPAQUE_FUNCTIONS = 0x2485, "define_opaque_functions";
        DEFINE_OPAQUE_AT = 0x2486, "define_opaque_at";

        // Inputs.
        FROZEN_IMPLS = 0x25e0, "FrozenImpls";

        // Interned ids.
        INTERNED_OPAQUE_TY = 0x25f0, "InternedOpaqueTyId";
        INTERNED_CLOSURE = 0x25f1, "InternedClosureId";
        INTERNED_COROUTINE = 0x25f2, "InternedCoroutineId";
        INTERNED_COROUTINE_CLOSURE = 0x25f3, "InternedCoroutineClosureId";
        ANON_CONST = 0x25f4, "AnonConstId";
    }

    #[cfg(test)]
    mod tests {
        use std::path::Path;

        use rustc_hash::FxHashSet;

        use super::*;

        #[test]
        fn kinds_are_distinct_and_hir_tys() {
            assert!(blaze_query::kinds::FRONT.start <= RANGE.start);
            assert!(RANGE.end <= blaze_query::kinds::FRONT.end);
            let mut numbers = FxHashSet::default();
            let mut names = FxHashSet::default();
            for &(kind, name) in ALL {
                assert!(RANGE.contains(&kind.0), "{name}: {:#x} is not hir-ty's", kind.0);
                assert!(numbers.insert(kind), "{name}: {:#x} is given twice", kind.0);
                assert!(names.insert(name), "{name} has two kinds");
                assert_eq!(of(name), Some(kind));
                assert_eq!(super::name(kind), Some(name));
            }
            assert_eq!(of("no_such_query"), None);
            assert_eq!(super::name(QueryKind(RANGE.end - 1)), None);
        }

        /// The queries and interned ids declared in the source of hir-ty, as [`ALL`] names them.
        fn declared() -> Vec<String> {
            let mut found = Vec::new();
            let mut dirs = vec![Path::new(env!("CARGO_MANIFEST_DIR")).join("src")];
            while let Some(dir) = dirs.pop() {
                for entry in std::fs::read_dir(&dir).unwrap() {
                    let path = entry.unwrap().path();
                    if path.is_dir() {
                        dirs.push(path);
                    } else if path.extension().is_some_and(|it| it == "rs") {
                        scan(&std::fs::read_to_string(&path).unwrap(), &mut found);
                    }
                }
            }
            found
        }

        /// What follows each salsa attribute of `text`: a function, a method of a tracked
        /// `impl`, or an interned or input struct.
        fn scan(text: &str, found: &mut Vec<String>) {
            let attribute = concat!("#[", "salsa::");
            let mut in_impl: Option<String> = None;
            let mut lines = text.lines();
            while let Some(line) = lines.next() {
                if line.starts_with('}') {
                    in_impl = None;
                }
                let Some(rest) = line.trim_start().strip_prefix(attribute) else { continue };
                let keyword = match () {
                    _ if rest.starts_with("tracked") => "fn",
                    _ if rest.starts_with("interned") || rest.starts_with("input") => "struct",
                    _ => continue,
                };
                for next in lines.by_ref() {
                    let next = next.trim_start();
                    if next.starts_with("//") || next.starts_with("#[") {
                        continue;
                    }
                    if let Some(ty) = next.strip_prefix("impl") {
                        in_impl = Some(ident(ty));
                        break;
                    }
                    let mut words = next.split_whitespace();
                    if words.any(|it| it == keyword) {
                        let name = ident(words.next().unwrap_or_default());
                        found.push(match (keyword, &in_impl) {
                            ("fn", Some(ty)) => format!("{ty}::{name}"),
                            _ => name,
                        });
                        break;
                    }
                }
            }
        }

        /// The name `text` starts with, after the generic parameters of an `impl`.
        fn ident(text: &str) -> String {
            let text = text.trim_start();
            let text = match text.strip_prefix('<') {
                Some(rest) => &rest[rest.find('>').map_or(0, |at| at + 1)..],
                None => text,
            };
            text.trim_start().chars().take_while(|&c| c.is_alphanumeric() || c == '_').collect()
        }

        #[test]
        fn every_query_has_a_kind() {
            let declared = declared();
            for name in &declared {
                assert!(of(name).is_some(), "`{name}` has no kind in `hir_ty::db::kinds`");
            }
            for &(_, name) in ALL {
                assert!(declared.iter().any(|it| it == name), "`{name}` is no query of hir-ty");
            }
        }
    }
}
