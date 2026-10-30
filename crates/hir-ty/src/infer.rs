//! Type inference, i.e. the process of walking through the code and determining
//! the type of each expression and pattern.
//!
//! For type inference, compare the implementations in rustc (the various
//! check_* methods in [`rustc_hir_typeck/check.rs`] are a good entry point) and
//! IntelliJ-Rust (org.rust.lang.core.types.infer). Our entry point for
//! inference here is the `infer` function, which infers the types of all
//! expressions in a given function.
//!
//! During inference, types (i.e. the `Ty` struct) can contain type 'variables'
//! which represent currently unknown types; as we walk through the expressions,
//! we might determine that certain variables need to be equal to each other, or
//! to certain types. To record this, we use the union-find implementation from
//! the `ena` crate, which is extracted from rustc.
//!
//! [`rustc_hir_typeck/check.rs`]: https://github.com/rust-lang/rust/blob/5503df87342a73d0c29126a7e08dc9c1255c46ad/compiler/rustc_hir_typeck/src/check.rs

#![forbid(unsafe_code)]

mod ambiguity;
mod at_error;
mod autoderef;
mod callee;
pub mod cast;
pub mod closure;
mod coerce;
pub mod columns;
pub(crate) mod diagnostics;
mod expr;
mod fallback;
mod mutability;
mod op;
mod opaques;
mod pat;
mod path;
pub mod persist;
mod place_op;
pub mod prelude_lints;
mod sized_bound;
pub(crate) mod unify;

use std::{
    cell::{OnceCell, RefCell},
    cmp,
    convert::identity,
    fmt,
    hash::{Hash, Hasher},
    ops::Deref,
};

use base_db::{Db, Crate, FxIndexMap};
use either::Either;
use hir_def::{
    AdtId, AssocItemId, AttrDefId, DefWithBodyId, ExpressionStoreOwnerId, FieldId, FunctionId,
    GenericDefId, GenericParamId, HasModule, LocalFieldId, Lookup, TraitId, TupleFieldId, TupleId,
    VariantId,
    attrs::AttrFlags,
    expr_store::{Body, ExpressionStore, HygieneId, body::Param, path::Path},
    hir::{
        BinaryOp, BindingId, Expr, ExprId, ExprOrPatId, ExprOrPatIdPacked, LabelId, PatId, UnaryOp,
    },
    lang_item::LangItems,
    layout::Integer,
    resolver::{HasResolver, ResolveValueResult, Resolver, TypeNs, ValueNs},
    signatures::EnumSignature,
    type_ref::{LifetimeRefId, TypeRefId},
    unstable_features::UnstableFeatures,
};
use hir_expand::{mod_path::ModPath, name::Name};
use indexmap::IndexSet;
use la_arena::ArenaMap;
use macros::{TypeFoldable, TypeVisitable};
use rustc_ast_ir::Mutability;
use rustc_hash::{FxHashMap, FxHashSet};
use rustc_type_ir::{
    AliasTyKind, TypeFoldable, TypeVisitableExt,
    inherent::{GenericArgs as _, IntoKind, Ty as _},
};
use salsa::SalsaValue;
use smallvec::SmallVec;
use span::Edition;
use stdx::never;
use thin_vec::ThinVec;

use crate::{
    ImplTraitId, IncorrectGenericsLenKind, InferBodyId, PathLoweringDiagnostic, Span,
    TargetFeatures, ValueTyDefId,
    closure_analysis::PlaceBase,
    consteval::{create_anon_const, path_to_const},
    db::{AnonConstId, HirDatabase, InternedOpaqueTyId},
    generics::Generics,
    infer::{
        callee::DeferredCallResolution,
        closure::analysis::{
            BorrowKind,
            expr_use_visitor::{FakeReadCause, Place},
        },
        coerce::{CoerceMany, DynamicCoerceMany},
        columns::{
            Csr, CsrBuilder, ExprOrPatColumn, ExprOrPatMap, ExprOrPatSet, FieldColumn, IdxSet,
            IndexColumn, ModeColumn, SortedMap, TyColumn,
        },
        diagnostics::{
            Diagnostics, InferenceTyLoweringContext as TyLoweringContext,
            InferenceTyLoweringVarsCtx,
        },
        expr::ExprIsRead,
        pat::PatOrigin,
        unify::resolve_completely::WriteBackCtxt,
    },
    lower::{
        ImplTraitIdx, LifetimeElisionKind, LifetimeLoweringMode, LoweringMode,
        diagnostics::TyLoweringDiagnostic,
    },
    method_resolution::CandidateId,
    next_solver::{
        AliasConstKind, AliasTy, AnonConstIdWrapper, Const, ConstKind, DbInterner, ErrorGuaranteed,
        GenericArgs, IsRigid, Region, RegionExt, StoredFnSig, StoredGenericArg, StoredGenericArgs,
        StoredTy, StoredTys, Term, Ty, TyKind, Tys,
        abi::Safety,
        infer::{InferCtxt, ObligationInspector, traits::ObligationCause},
    },
    solver_errors::SolverDiagnostic,
    solver_errors::{Ambiguity, TypeAnnotationNeeded},
    solver_errors::StoredGoal,
    utils::TargetFeatureIsSafeInTarget,
};

pub use coerce::could_coerce;
pub use unify::{could_unify, could_unify_deeply};

use cast::{CastCheck, CastError, CastPointers};

/// The entry point of type inference.
fn infer_query<'db>(db: &'db Db, def: DefWithBodyId) -> InferenceResult<'db> {
    infer_query_with_inspect(db, def, None, LoweringMode::Analysis)
}

pub fn infer_query_with_inspect<'db>(
    db: &'db Db,
    def: DefWithBodyId,
    inspect: Option<ObligationInspector<'db>>,
    lowering_mode: LoweringMode,
) -> InferenceResult<'db> {
    let _p = tracing::info_span!("infer_query").entered();
    let resolver = def.resolver(db);
    let body = Body::of(db, def);
    let mut ctx = InferenceContext::new(
        db,
        InferBodyId::DefWithBodyId(def),
        ExpressionStoreOwnerId::Body(def),
        def.generic_def(db),
        &body.store,
        resolver,
        true,
        lowering_mode,
    );

    if let Some(inspect) = inspect {
        ctx.table.infer_ctxt.attach_obligation_inspector(inspect);
    }

    match def {
        DefWithBodyId::FunctionId(f) => {
            ctx.collect_fn(f, body.self_param.map(|param| param.formal), &body.params)
        }
        DefWithBodyId::ConstId(c) => ctx.collect_const_or_static(c.into()),
        DefWithBodyId::StaticId(s) => ctx.collect_const_or_static(s.into()),
        DefWithBodyId::VariantId(v) => {
            ctx.return_ty = match EnumSignature::variant_body_type(db, v.lookup(db).parent) {
                hir_def::layout::IntegerType::Pointer(signed) => match signed {
                    true => ctx.types.types.isize,
                    false => ctx.types.types.usize,
                },
                hir_def::layout::IntegerType::Fixed(size, signed) => match signed {
                    true => match size {
                        Integer::I8 => ctx.types.types.i8,
                        Integer::I16 => ctx.types.types.i16,
                        Integer::I32 => ctx.types.types.i32,
                        Integer::I64 => ctx.types.types.i64,
                        Integer::I128 => ctx.types.types.i128,
                    },
                    false => match size {
                        Integer::I8 => ctx.types.types.u8,
                        Integer::I16 => ctx.types.types.u16,
                        Integer::I32 => ctx.types.types.u32,
                        Integer::I64 => ctx.types.types.u64,
                        Integer::I128 => ctx.types.types.u128,
                    },
                },
            };
        }
    }

    ctx.infer_body(body.root_expr());

    ctx.infer_mut_body(body.root_expr());

    infer_finalize(ctx)
}

fn infer_cycle_result<'db>(
    db: &'db Db,
    _: salsa::Id,
    _: DefWithBodyId,
) -> InferenceResult<'db> {
    InferenceResult {
        has_errors: true,
        ..InferenceResult::new(Ty::new_error(DbInterner::new_no_crate(db), ErrorGuaranteed))
    }
}

/// Infer types for an anonymous const expression.
fn infer_anon_const_query<'db>(
    db: &'db Db,
    def: AnonConstId<'db>,
) -> InferenceResult<'db> {
    let _p = tracing::info_span!("infer_anon_const_query").entered();
    let loc = def.loc(db);
    let store_owner = loc.owner;
    let store = ExpressionStore::of(db, store_owner);

    let resolver = store_owner.resolver(db);

    let mut ctx = InferenceContext::new(
        db,
        InferBodyId::AnonConstId(def),
        store_owner,
        loc.owner.generic_def(db),
        store,
        resolver,
        loc.allow_using_generic_params,
        LoweringMode::Analysis,
    );

    ctx.infer_expr(
        loc.expr,
        &Expectation::has_type(loc.ty.get().instantiate_identity().skip_norm_wip()),
        ExprIsRead::Yes,
    );

    infer_finalize(ctx)
}

fn infer_anon_const_cycle_result<'db>(
    db: &'db Db,
    _: salsa::Id,
    _: AnonConstId<'db>,
) -> InferenceResult<'db> {
    InferenceResult {
        has_errors: true,
        ..InferenceResult::new(Ty::new_error(DbInterner::new_no_crate(db), ErrorGuaranteed))
    }
}

fn infer_finalize<'db>(mut ctx: InferenceContext<'db>) -> InferenceResult<'db> {
    ctx.handle_opaque_type_uses();

    ctx.type_inference_fallback();

    // Comment from rustc:
    // Even though coercion casts provide type hints, we check casts after fallback for
    // backwards compatibility. This makes fallback a stronger type hint than a cast coercion.
    let cast_checks = std::mem::take(&mut ctx.deferred_cast_checks);
    for mut cast in cast_checks.into_iter() {
        if let Err(diag) = cast.check(&mut ctx) {
            ctx.diagnostics.push(diag);
        }
    }

    ctx.table.select_obligations_where_possible();

    // Closure and coroutine analysis may run after fallback
    // because they don't constrain other type variables.
    ctx.closure_analyze();
    assert!(ctx.deferred_call_resolutions.is_empty());

    ctx.table.select_obligations_where_possible();

    ctx.handle_opaque_type_uses();

    ctx.report_ambiguity_errors();

    ctx.merge_anon_consts();

    ctx.resolve_all()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub enum ByRef {
    Yes(Mutability),
    No,
}

/// The mode of a binding (`mut`, `ref mut`, etc).
/// Used for both the explicit binding annotations given in the HIR for a binding
/// and the final binding mode that we infer after type inference/match ergonomics.
/// `.0` is the by-reference mode (`ref`, `ref mut`, or by value),
/// `.1` is the mutability of the binding.
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub struct BindingMode(pub ByRef, pub Mutability);

#[derive(Debug, PartialEq, Eq, Clone, Hash, TypeVisitable, TypeFoldable)]
pub enum InferenceDiagnostic {
    NoSuchField {
        #[type_visitable(ignore)]
        field: ExprOrPatIdPacked,
        #[type_visitable(ignore)]
        private: Option<LocalFieldId>,
        #[type_visitable(ignore)]
        variant: VariantId,
    },
    MismatchedArrayPatLen {
        #[type_visitable(ignore)]
        pat: PatId,
        #[type_visitable(ignore)]
        expected: u64,
        #[type_visitable(ignore)]
        found: u64,
        #[type_visitable(ignore)]
        has_rest: bool,
    },
    ArrayPatternWithoutFixedLength {
        #[type_visitable(ignore)]
        pat: PatId,
    },
    ExpectedArrayOrSlicePat {
        #[type_visitable(ignore)]
        pat: PatId,
        found: StoredTy,
    },
    InvalidRangePatType {
        #[type_visitable(ignore)]
        pat: PatId,
        /// Each end's type as inference had it when it checked that end (see
        /// `types_at_error`), and whether that end is not `char` or numeric.
        #[type_visitable(ignore)]
        lhs: Option<(StoredTy, bool)>,
        #[type_visitable(ignore)]
        rhs: Option<(StoredTy, bool)>,
    },
    DuplicateField {
        #[type_visitable(ignore)]
        field: ExprOrPatIdPacked,
        #[type_visitable(ignore)]
        variant: VariantId,
    },
    PrivateField {
        #[type_visitable(ignore)]
        expr: ExprId,
        #[type_visitable(ignore)]
        field: FieldId,
    },
    PrivateAssocItem {
        #[type_visitable(ignore)]
        id: ExprOrPatIdPacked,
        #[type_visitable(ignore)]
        item: AssocItemId,
    },
    UnresolvedField {
        #[type_visitable(ignore)]
        expr: ExprId,
        receiver: StoredTy,
        #[type_visitable(ignore)]
        name: Name,
        #[type_visitable(ignore)]
        method_with_same_name_exists: bool,
    },
    UnresolvedMethodCall {
        #[type_visitable(ignore)]
        expr: ExprId,
        receiver: StoredTy,
        #[type_visitable(ignore)]
        name: Name,
        /// Contains the type the field resolves to
        field_with_same_name: Option<StoredTy>,
        #[type_visitable(ignore)]
        assoc_func_with_same_name: Option<FunctionId>,
    },
    UnresolvedAssocItem {
        #[type_visitable(ignore)]
        id: ExprOrPatIdPacked,
        /// The type the item was looked up on.
        ty: StoredTy,
        #[type_visitable(ignore)]
        name: Name,
    },
    UnresolvedIdent {
        #[type_visitable(ignore)]
        id: ExprOrPatIdPacked,
    },
    // FIXME: This should be emitted in body lowering
    BreakOutsideOfLoop {
        #[type_visitable(ignore)]
        expr: ExprId,
        #[type_visitable(ignore)]
        is_break: bool,
        #[type_visitable(ignore)]
        bad_value_break: bool,
    },
    /// E0695: an unlabeled `break` or `continue` whose innermost labeled block, loop or closure
    /// is a labeled block. It still jumps to the loop for typing.
    UnlabeledJumpInLabeledBlock {
        #[type_visitable(ignore)]
        expr: ExprId,
        #[type_visitable(ignore)]
        is_break: bool,
    },
    /// E0727: a `yield` in an `async` block, closure or function.
    AsyncCoroutinesNotSupported {
        #[type_visitable(ignore)]
        expr: ExprId,
    },
    NonExhaustiveRecordExpr {
        #[type_visitable(ignore)]
        expr: ExprId,
    },
    NonExhaustiveRecordPat {
        #[type_visitable(ignore)]
        pat: PatId,
        #[type_visitable(ignore)]
        variant: VariantId,
    },
    UnionPatMustHaveExactlyOneField {
        #[type_visitable(ignore)]
        pat: PatId,
    },
    UnionPatHasRest {
        #[type_visitable(ignore)]
        pat: PatId,
    },
    FunctionalRecordUpdateOnNonStruct {
        #[type_visitable(ignore)]
        base_expr: ExprId,
    },
    MismatchedArgCount {
        #[type_visitable(ignore)]
        call_expr: ExprId,
        #[type_visitable(ignore)]
        expected: usize,
        #[type_visitable(ignore)]
        found: usize,
        /// True when the call goes through the `Fn`/`FnMut`/`FnOnce` trait
        /// (i.e. arguments were bundled into a tuple). Determines whether the
        /// diagnostic surface uses E0057 (Fn-trait call) or E0061 (regular call).
        #[type_visitable(ignore)]
        is_fn_trait_call: bool,
        #[type_visitable(ignore)]
        args: Box<CallArguments>,
    },
    /// rustc's `report_arg_errors` of a call whose argument count fits but some argument
    /// does not (E0308 "arguments to this function are incorrect"); each such argument also
    /// has its `TypeMismatch`.
    IncorrectArguments {
        #[type_visitable(ignore)]
        call_expr: ExprId,
        #[type_visitable(ignore)]
        args: Box<CallArguments>,
    },
    MismatchedTupleStructPatArgCount {
        #[type_visitable(ignore)]
        pat: PatId,
        #[type_visitable(ignore)]
        expected: usize,
        #[type_visitable(ignore)]
        found: usize,
        #[type_visitable(ignore)]
        variant: VariantId,
        /// Whether the pattern's type did not match the scrutinee's (rustc's `had_err`).
        #[type_visitable(ignore)]
        had_err: bool,
    },
    ExpectedFunction {
        #[type_visitable(ignore)]
        call_expr: ExprId,
        found: StoredTy,
    },
    CannotBeDereferenced {
        #[type_visitable(ignore)]
        expr: ExprId,
        found: StoredTy,
        /// `found` as inference had it at the error, as rustc prints it (`{integer}`,
        /// `{float}`; see `TypeMismatch::at_error`). `None` when it held no such literal's type.
        #[type_visitable(ignore)]
        at_error: Option<StoredTy>,
    },
    UnaryOperatorCannotBeApplied {
        #[type_visitable(ignore)]
        expr: ExprId,
        #[type_visitable(ignore)]
        op: UnaryOp,
        found: StoredTy,
        /// `found` as inference had it at the error, as rustc prints it (`{integer}`,
        /// `{float}`; see `TypeMismatch::at_error`). `None` when it held no such literal's type.
        #[type_visitable(ignore)]
        at_error: Option<StoredTy>,
        /// rustc's `errors` of `check_user_unop`: the leaf goal of each fulfillment error of
        /// `found: Trait`, empty when the trait is missing or the goal holds.
        unmet: Vec<StoredGoal>,
    },
    BinaryOperatorCannotBeApplied {
        #[type_visitable(ignore)]
        expr: ExprId,
        #[type_visitable(ignore)]
        op: BinaryOp,
        lhs: StoredTy,
        rhs: StoredTy,
        /// `lhs` and `rhs` as inference had them at the error, as rustc prints them
        /// ("cannot add `{integer}` to `bool`"; see `TypeMismatch::at_error`).
        #[type_visitable(ignore)]
        at_error: Option<Box<(StoredTy, StoredTy)>>,
        /// rustc's `errors` of `check_overloaded_binop`: the leaf goal of each fulfillment
        /// error of `lhs: Trait<rhs>` (`X: PartialEq` for `[X; 3] == [X; 3]`), empty when the
        /// trait is missing or the goal holds ("the trait `..` is not implemented for").
        unmet: Vec<StoredGoal>,
        /// `ObligationCauseCode::BinOp`'s `output_ty`: the type expected of the operation.
        output: Option<StoredTy>,
    },
    MutRefInImmRefPat {
        #[type_visitable(ignore)]
        pat: PatId,
    },
    CannotImplicitlyDerefTraitObject {
        #[type_visitable(ignore)]
        pat: PatId,
        found: StoredTy,
    },
    CannotIndexInto {
        #[type_visitable(ignore)]
        expr: ExprId,
        found: StoredTy,
        /// `found` as inference had it at the error, as rustc prints it (`{integer}`,
        /// `{float}`; see `TypeMismatch::at_error`). `None` when it held no such literal's type.
        #[type_visitable(ignore)]
        at_error: Option<StoredTy>,
    },
    TypedHole {
        #[type_visitable(ignore)]
        expr: ExprId,
        expected: StoredTy,
    },
    CastToUnsized {
        #[type_visitable(ignore)]
        expr: ExprId,
        cast_ty: StoredTy,
    },
    InvalidCast {
        #[type_visitable(ignore)]
        expr: ExprId,
        #[type_visitable(ignore)]
        error: CastError,
        expr_ty: StoredTy,
        cast_ty: StoredTy,
        /// The pointer kinds rustc's report of a pointer cast reads.
        #[type_visitable(ignore)]
        pointers: Option<Box<CastPointers>>,
    },
    TyDiagnostic {
        #[type_visitable(ignore)]
        diag: TyLoweringDiagnostic,
    },
    PathDiagnostic {
        #[type_visitable(ignore)]
        node: ExprOrPatIdPacked,
        #[type_visitable(ignore)]
        diag: PathLoweringDiagnostic,
    },
    MethodCallIncorrectGenericsLen {
        #[type_visitable(ignore)]
        expr: ExprId,
        #[type_visitable(ignore)]
        provided_count: u32,
        #[type_visitable(ignore)]
        expected_count: u32,
        #[type_visitable(ignore)]
        kind: IncorrectGenericsLenKind,
        #[type_visitable(ignore)]
        def: GenericDefId,
    },
    MethodCallIllegalSizedBound {
        #[type_visitable(ignore)]
        call_expr: ExprId,
        #[type_visitable(ignore)]
        method: FunctionId,
        /// Whose own clauses (the method's, or its trait's or impl's) hold the `Self: Sized`
        /// bound, as rustc's span of it; `None` where only elaboration gives it.
        #[type_visitable(ignore)]
        bound: Option<GenericDefId>,
        #[type_visitable(ignore)]
        needs_mut: bool,
        /// The traits a probe over every crate's traits picks the method from instead.
        #[type_visitable(ignore)]
        candidates: Box<[TraitId]>,
    },
    /// rustc's `lint_edition_dependent_dot_call`: a method call `lint`'s edition resolves to
    /// another method.
    EditionDependentMethodCall {
        #[type_visitable(ignore)]
        call_expr: ExprId,
        #[type_visitable(ignore)]
        receiver: ExprId,
        #[type_visitable(ignore)]
        lint: prelude_lints::EditionLint,
        #[type_visitable(ignore)]
        method: FunctionId,
        /// An inherent or object pick, whose receiver rustc writes with its adjustments; else
        /// it writes the call fully qualified.
        #[type_visitable(ignore)]
        inherent: bool,
        #[type_visitable(ignore)]
        autoderefs: u32,
        #[type_visitable(ignore)]
        autoref: Option<prelude_lints::ReceiverAdjustment>,
    },
    /// rustc's `lint_fully_qualified_call_from_2018`: `T::try_from`, `T::try_into` or
    /// `T::from_iter` of a trait the 2021 prelude adds one of that name beside.
    FullyQualifiedCallFrom2018 {
        #[type_visitable(ignore)]
        node: ExprOrPatIdPacked,
        #[type_visitable(ignore)]
        item: CandidateId,
        self_ty: StoredTy,
    },
    MethodCallIncorrectGenericsOrder {
        #[type_visitable(ignore)]
        expr: ExprId,
        #[type_visitable(ignore)]
        param_id: GenericParamId,
        #[type_visitable(ignore)]
        arg_idx: u32,
        /// Whether the `GenericArgs` contains a `Self` arg.
        #[type_visitable(ignore)]
        has_self_arg: bool,
    },
    /// Lifetime arguments to a method with late-bound lifetimes: the
    /// `late_bound_lifetime_arguments` lint; they are ignored.
    MethodCallLateBoundLifetimeArgs {
        #[type_visitable(ignore)]
        expr: ExprId,
        #[type_visitable(ignore)]
        def: GenericDefId,
    },
    InvalidLhsOfAssignment {
        #[type_visitable(ignore)]
        lhs: ExprId,
        /// The `=` or compound assignment `lhs` is the left-hand side of.
        #[type_visitable(ignore)]
        assign: ExprId,
        /// `None` for `=`, E0070; the operator of a compound assignment, E0067.
        #[type_visitable(ignore)]
        op: Option<hir_def::hir::ArithOp>,
    },
    TypeMustBeKnown {
        #[type_visitable(ignore)]
        at_point: Span,
        top_term: Option<StoredGenericArg>,
        #[type_visitable(ignore)]
        kind: TypeAnnotationNeeded,
        /// The goal left ambiguous, for an error of a pending goal (rustc's
        /// `maybe_report_ambiguity`); `None` where writeback or `structurally_resolve_type`
        /// found the type undecided.
        ambiguity: Option<Box<Ambiguity>>,
    },
    UnionExprMustHaveExactlyOneField {
        #[type_visitable(ignore)]
        expr: ExprId,
    },
    TypeMismatch {
        #[type_visitable(ignore)]
        node: ExprOrPatIdPacked,
        expected: StoredTy,
        found: StoredTy,
        /// `expected` and `found` as inference had them at the error, as rustc prints them:
        /// the type of an integer or float literal not decided yet a `FreshIntTy` /
        /// `FreshFloatTy` (`{integer}`, `{float}`), any other variable an error. `None` when
        /// neither held such a literal's type. For printing only: no solver may see it.
        #[type_visitable(ignore)]
        at_error: Option<Box<(StoredTy, StoredTy)>>,
    },
    SolverDiagnostic(SolverDiagnostic),
    ExplicitDropMethodUse {
        #[type_visitable(ignore)]
        kind: ExplicitDropMethodUseKind,
    },
    MutableRefBinding {
        #[type_visitable(ignore)]
        pat: PatId,
    },
    YieldOutsideCoroutine {
        #[type_visitable(ignore)]
        expr: ExprId,
    },
    ReturnOutsideFunction {
        #[type_visitable(ignore)]
        expr: ExprId,
        #[type_visitable(ignore)]
        kind: ReturnKind,
    },
    RecordMissingFields {
        #[type_visitable(ignore)]
        record: ExprOrPatId,
        #[type_visitable(ignore)]
        variant: VariantId,
        #[type_visitable(ignore)]
        missed_fields: Vec<LocalFieldId>,
    },
}

impl InferenceDiagnostic {
    /// Whether this is an error that leaves the body tainted by errors, like rustc's
    /// `InferCtxt::tainted_by_errors`: once it is, "type annotations needed" is not reported.
    pub(crate) fn taints_body(&self) -> bool {
        let is_lint = |diag: &PathLoweringDiagnostic| {
            matches!(diag, PathLoweringDiagnostic::ElidedLifetimesInPath { hard_error: false, .. })
        };
        match self {
            InferenceDiagnostic::TypeMustBeKnown { ambiguity, .. } => ambiguity.is_some(),
            InferenceDiagnostic::PathDiagnostic { diag, .. }
            | InferenceDiagnostic::TyDiagnostic {
                diag: TyLoweringDiagnostic::PathDiagnostic { diag, .. },
            } => !is_lint(diag),
            _ => true,
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Hash)]
pub enum ReturnKind {
    ReturnExpr,
    BecomeExpr,
}

#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub enum ExplicitDropMethodUseKind {
    MethodCall(ExprId),
    Path(ExprOrPatIdPacked),
}

/// What rustc's `warn_if_unreachable` (`rustc_hir_typeck/src/fn_ctxt/_impl.rs`) warns of: code
/// type checking reaches after an expression that diverges (`unreachable_code`).
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Unreachable {
    pub what: UnreachableAt,
    /// What diverged: `Diverges::Always`'s `span` (an expression, or a never pattern).
    pub after: ExprOrPatId,
    /// `Diverges::Always`'s `custom_note` ([`Unreachable::ALL_ARMS_DIVERGE`]), `None` for
    /// "any code following this expression is unreachable".
    pub note: Option<&'static str>,
}

impl Unreachable {
    /// The note of a `match` whose arms all diverge.
    pub const ALL_ARMS_DIVERGE: &'static str =
        "any code following this `match` expression is unreachable, as all arms diverge";
}

/// rustc's `id`, `span` and `kind` of `warn_if_unreachable`, by its callers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnreachableAt {
    /// "expression": `check_expr_with_expectation_and_args`, before an expression and after one
    /// whose children diverged, and `check_argument_types` for an argument.
    Expr(ExprId),
    /// "call": a call whose callee or arguments diverged (`ExprKind::Call`/`MethodCall`).
    Call(ExprId),
    /// "statement": `check_stmt`, statement `index` of the block.
    Stmt { block: ExprId, index: u32 },
    /// "arm": `warn_arms_when_scrutinee_diverges`, the body of arm `index` of the `match`.
    Arm { match_expr: ExprId, index: u32 },
    /// "block in `if` or `while` expression": `check_expr_if`, after its condition diverged.
    IfBlock(ExprId),
    /// "block in `let` expression": `check_expr_let`, its initializer.
    LetInit(ExprId),
}

/// What rustc's `report_arg_errors` reads of a call whose arguments do not fit its inputs
/// (`check_argument_types` with `call_appears_satisfied` false). Every type is as found
/// then: an undecided integer or float literal's type a `FreshIntTy` / `FreshFloatTy`
/// (`{integer}`, `{float}`), any other variable an error. For printing only: no solver may
/// see them.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct CallArguments {
    /// rustc's `callee_ty`: the adjusted type of a call's callee (a function's, constructor's
    /// or method's `FnDef`, a closure, a function pointer, or a type called through
    /// `Fn`/`FnMut`/`FnOnce`), or of a method call's receiver.
    pub callee: StoredTy,
    /// The method of a method call (rustc's `type_dependent_def`); `None` for a call.
    pub method: Option<FunctionId>,
    /// A method call (rustc's `is_method`): the receiver is not among the arguments.
    pub is_method: bool,
    /// The callee is C-variadic (E0060).
    pub c_variadic: bool,
    /// `formal_and_expected_inputs`: the signature's inputs (untupled for a `Fn*` call).
    pub formal_inputs: Box<[StoredTy]>,
    /// What the expected output makes of `formal_inputs`, the same when it makes nothing.
    pub expected_inputs: Box<[StoredTy]>,
    /// `provided_arg_tys`: each argument's type after adjustments, in order, without those
    /// taken as legacy const generics (all of them past the inputs of a C-variadic callee).
    pub provided: Box<[StoredTy]>,
    /// The compatible cells of `ArgMatrix::new`, by provided argument: argument `i` fits the
    /// inputs `compatible[compatible_ends[i - 1]..compatible_ends[i]]`, the first from 0.
    pub compatible_ends: Box<[u32]>,
    pub compatible: Box<[u32]>,
}

impl CallArguments {
    /// The inputs provided argument `provided` fits (`Compatibility::Compatible`).
    pub fn compatible_with(&self, provided: usize) -> &[u32] {
        let start = provided.checked_sub(1).map_or(0, |it| self.compatible_ends[it] as usize);
        &self.compatible[start..self.compatible_ends[provided] as usize]
    }
}

/// Represents coercing a value to a different type of value.
///
/// We transform values by following a number of `Adjust` steps in order.
/// See the documentation on variants of `Adjust` for more details.
///
/// Here are some common scenarios:
///
/// 1. The simplest cases are where a pointer is not adjusted fat vs thin.
///    Here the pointer will be dereferenced N times (where a dereference can
///    happen to raw or borrowed pointers or any smart pointer which implements
///    Deref, including Box<_>). The types of dereferences is given by
///    `autoderefs`. It can then be auto-referenced zero or one times, indicated
///    by `autoref`, to either a raw or borrowed pointer. In these cases unsize is
///    `false`.
///
/// 2. A thin-to-fat coercion involves unsizing the underlying data. We start
///    with a thin pointer, deref a number of times, unsize the underlying data,
///    then autoref. The 'unsize' phase may change a fixed length array to a
///    dynamically sized one, a concrete object to a trait object, or statically
///    sized struct to a dynamically sized one. E.g., &[i32; 4] -> &[i32] is
///    represented by:
///
///    ```ignore
///    Deref(None) -> [i32; 4],
///    Borrow(AutoBorrow::Ref) -> &[i32; 4],
///    Unsize -> &[i32],
///    ```
///
///    Note that for a struct, the 'deep' unsizing of the struct is not recorded.
///    E.g., `struct Foo<T> { it: T }` we can coerce &Foo<[i32; 4]> to &Foo<[i32]>
///    The autoderef and -ref are the same as in the above example, but the type
///    stored in `unsize` is `Foo<[i32]>`, we don't store any further detail about
///    the underlying conversions from `[i32; 4]` to `[i32]`.
///
/// 3. Coercing a `Box<T>` to `Box<dyn Trait>` is an interesting special case. In
///    that case, we have the pointer we need coming in, so there are no
///    autoderefs, and no autoref. Instead we just do the `Unsize` transformation.
///    At some point, of course, `Box` should move out of the compiler, in which
///    case this is analogous to transforming a struct. E.g., Box<[i32; 4]> ->
///    Box<[i32]> is an `Adjust::Unsize` with the target `Box<[i32]>`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Adjustment {
    pub kind: Adjust,
    pub target: StoredTy,
}

impl Adjustment {
    pub fn borrow<'db>(
        interner: DbInterner<'db>,
        m: Mutability,
        ty: Ty<'db>,
        lt: Region<'db>,
    ) -> Self {
        let ty = Ty::new_ref(interner, lt, ty, m);
        Adjustment {
            kind: Adjust::Borrow(AutoBorrow::Ref(AutoBorrowMutability::new(m, AllowTwoPhase::No))),
            target: ty.store(),
        }
    }
}

/// At least for initial deployment, we want to limit two-phase borrows to
/// only a few specific cases. Right now, those are mostly "things that desugar"
/// into method calls:
/// - using `x.some_method()` syntax, where some_method takes `&mut self`,
/// - using `Foo::some_method(&mut x, ...)` syntax,
/// - binary assignment operators (`+=`, `-=`, `*=`, etc.).
///
/// Anything else should be rejected until generalized two-phase borrow support
/// is implemented. Right now, dataflow can't handle the general case where there
/// is more than one use of a mutable borrow, and we don't want to accept too much
/// new code via two-phase borrows, so we try to limit where we create two-phase
/// capable mutable borrows.
/// See #49434 for tracking.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum AllowTwoPhase {
    // FIXME: We should use this when appropriate.
    Yes,
    No,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Adjust {
    /// Go from ! to any type.
    NeverToAny,
    /// Dereference once, producing a place.
    Deref(Option<OverloadedDeref>),
    /// Take the address and produce either a `&` or `*` pointer.
    Borrow(AutoBorrow),
    Pointer(PointerCast),
}

/// An overloaded autoderef step, representing a `Deref(Mut)::deref(_mut)`
/// call, with the signature `&'a T -> &'a U` or `&'a mut T -> &'a mut U`.
/// The target type is `U` in both cases, with the region and mutability
/// being those shared by both the receiver and the returned reference.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct OverloadedDeref(pub Mutability);

#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub enum AutoBorrowMutability {
    Mut { allow_two_phase_borrow: AllowTwoPhase },
    Not,
}

impl AutoBorrowMutability {
    /// Creates an `AutoBorrowMutability` from a mutability and allowance of two phase borrows.
    ///
    /// Note that when `mutbl.is_not()`, `allow_two_phase_borrow` is ignored
    pub fn new(mutbl: Mutability, allow_two_phase_borrow: AllowTwoPhase) -> Self {
        match mutbl {
            Mutability::Not => Self::Not,
            Mutability::Mut => Self::Mut { allow_two_phase_borrow },
        }
    }
}

impl From<AutoBorrowMutability> for Mutability {
    fn from(m: AutoBorrowMutability) -> Self {
        match m {
            AutoBorrowMutability::Mut { .. } => Mutability::Mut,
            AutoBorrowMutability::Not => Mutability::Not,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AutoBorrow {
    /// Converts from T to &T.
    Ref(AutoBorrowMutability),
    /// Converts from T to *T.
    RawPtr(Mutability),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PointerCast {
    /// Go from a fn-item type to a fn-pointer type.
    ReifyFnPointer,

    /// Go from a safe fn pointer to an unsafe fn pointer.
    UnsafeFnPointer,

    /// Go from a non-capturing closure to an fn pointer or an unsafe fn pointer.
    /// It cannot convert a closure that requires unsafe.
    ClosureFnPointer(Safety),

    /// Go from a mut raw pointer to a const raw pointer.
    MutToConstPointer,

    #[allow(dead_code)]
    /// Go from `*const [T; N]` to `*const T`
    ArrayToPointer,

    /// Unsize a pointer/reference value, e.g., `&[T; n]` to
    /// `&[T]`. Note that the source could be a thin or fat pointer.
    /// This will do things like convert thin pointers to fat
    /// pointers, or convert structs containing thin pointers to
    /// structs containing fat pointers, or convert between fat
    /// pointers. We don't store the details of how the transform is
    /// done (in fact, we don't know that, because it might depend on
    /// the precise type parameters). We just store the target
    /// type. Codegen backends and miri figure out what has to be done
    /// based on the precise source/target type at hand.
    Unsize,
}

/// Represents an implicit coercion applied to the scrutinee of a match before testing a pattern
/// against it. Currently, this is used only for implicit dereferences.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PatAdjustment {
    pub kind: PatAdjust,
    /// The type of the scrutinee before the adjustment is applied, or the "adjusted type" of the
    /// pattern.
    pub source: StoredTy,
}

/// Represents implicit coercions of patterns' types, rather than values' types.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Hash)]
pub enum PatAdjust {
    /// An implicit dereference before matching, such as when matching the pattern `0` against a
    /// scrutinee of type `&u8` or `&mut u8`.
    BuiltinDeref,
    /// An implicit call to `Deref(Mut)::deref(_mut)` before matching, such as when matching the
    /// pattern `[..]` against a scrutinee of type `Vec<T>`.
    OverloadedDeref,
}

/// The result of type inference: A mapping from expressions and patterns to types.
///
/// Columns (docs/v2/plan/design/data.md, [`columns`]): what each expression, pattern and
/// binding has is in a column indexed by its arena index, lists are CSR, sets bitsets, and
/// what few nodes have sits in side tables. Every column is canonical, so the derived `Eq`
/// and `Hash` compare and hash what the result says.
///
/// When you add a field that stores types (including `Substitution` and the like), don't forget
/// `resolve_completely()`'ing  them in `InferenceContext::resolve_all()`. Inference variables must
/// not appear in the final inference result.
#[derive(Clone, PartialEq, Eq, Hash, Debug, SalsaValue)]
pub struct InferenceResult<'db> {
    /// For each method call expr, records the function it resolves to.
    pub(crate) method_resolutions: IndexColumn<ExprId, (FunctionId, StoredGenericArgs)>,
    /// For each field access expr, records the field it resolves to.
    pub(crate) field_resolutions: FieldColumn,
    /// For each struct literal or pattern, records the variant it resolves to.
    pub(crate) variant_resolutions: ExprOrPatColumn<VariantId>,
    /// For each value path, expression or pattern, what it resolves to (an associated item's
    /// path as its function or const, which [`Self::assoc_resolutions`] gives with arguments).
    pub(crate) value_resolutions: ExprOrPatColumn<ValueNs>,
    /// For each associated item record what it resolves to
    pub(crate) assoc_resolutions: ExprOrPatColumn<(CandidateId, StoredGenericArgs)>,
    /// Whenever a tuple field expression access a tuple field, we allocate a tuple id in
    /// [`InferenceContext`] and store the tuples substitution there. This map is the reverse of
    /// that which allows us to resolve a [`TupleFieldId`]s type.
    pub(crate) tuple_field_access_types: Box<[StoredTys]>,

    pub type_of_expr: TyColumn<ExprId>,
    /// For each pattern record the type it resolves to.
    ///
    /// **Note**: When a pattern type is resolved it may still contain
    /// unresolved or missing subpatterns or subpatterns of mismatched types.
    pub type_of_pat: TyColumn<PatId>,
    pub type_of_binding: TyColumn<BindingId>,
    pub type_of_type_placeholder: TyColumn<TypeRefId>,
    pub type_of_opaque: SortedMap<InternedOpaqueTyId<'db>, StoredTy>,

    /// Whether there are any type-mismatching errors in the result.
    // FIXME: This isn't as useful as initially thought due to us falling back placeholders to
    // `TyKind::Error`.
    // Which will then mark this field.
    pub has_errors: bool,
    /// A side table: during inference [`InferenceContext::diagnostics`] is filled instead.
    pub(crate) diagnostics: Box<[InferenceDiagnostic]>,
    /// What is unreachable after an expression that diverges, in the order rustc warns of it.
    pub(crate) unreachable: Box<[Unreachable]>,
    pub(crate) nodes_with_type_mismatches: ExprOrPatSet,

    /// Interned `Error` type to return references to.
    // FIXME: Remove this.
    pub(crate) error_ty: StoredTy,

    pub expr_adjustments: Csr<ExprId, Adjustment>,
    /// For each place a destructuring assignment assigns to, the coercion of the part of the
    /// value it is given to the place's type: rustc's `place = lhsN` of
    /// `{ let (lhs0, ..) = rhs; place0 = lhs0; .. }` coerces `lhsN`. These are not adjustments of
    /// the place, which is used as it is written (a reborrow here does not make `g.label`
    /// a shared use of `g`).
    pub assignment_coercions: Csr<ExprId, Adjustment>,
    /// Stores the types which were implicitly dereferenced in pattern binding modes.
    pub pat_adjustments: Csr<PatId, PatAdjustment>,
    /// Stores the binding mode (`ref` in `let ref x = 2`) of bindings.
    ///
    /// This one is tied to the `PatId` instead of `BindingId`, because in some rare cases, a binding in an
    /// or pattern can have multiple binding modes. For example:
    /// ```
    /// fn foo(mut slice: &[u32]) -> usize {
    ///     slice = match slice {
    ///         [0, rest @ ..] | rest => rest,
    ///     };
    ///     0
    /// }
    /// ```
    /// the first `rest` has implicit `ref` binding mode, but the second `rest` binding mode is `move`.
    pub binding_modes: ModeColumn<PatId>,

    /// Set of reference patterns that match against a match-ergonomics inserted reference
    /// (as opposed to against a reference in the scrutinee type).
    pub(crate) skipped_ref_pats: IdxSet<PatId>,

    pub coercion_casts: IdxSet<ExprId>,

    /// Closures typed without an expected signature, from what their parameters and return
    /// type are written as: the lifetimes elided in a parameter's are the closure's own.
    pub closures_without_expectation: IdxSet<ExprId>,

    /// The type of each `sym` operand of an inline `asm!` that names a function, by the
    /// `asm!` and the operand's index: rustc type checks the path as a value, so a function's
    /// generic arguments (`sym dep::<'a, T>`) are those it spells out.
    pub asm_sym_tys: SortedMap<(ExprId, u32), StoredTy>,

    /// The capture analysis of each closure, a side table.
    pub closures_data: SortedMap<ExprId, ClosureData>,

    pub(crate) defined_anon_consts: Box<[AnonConstId<'db>]>,
}

#[cfg(target_pointer_width = "64")]
const _: () = {
    assert!(size_of::<Adjust>() <= 4);
    assert!(size_of::<Adjustment>() <= 16);
    assert!(size_of::<PatAdjustment>() <= 16);
    assert!(size_of::<BindingMode>() <= 2);
};

/// What inference has written so far: [`InferenceResult`]'s tables as the body is walked,
/// each written in place (dense arena maps, CSR lists that grow at the end of their flat
/// array, bitsets) and frozen into the result's columns by [`InferenceTables::freeze`].
#[derive(Clone, Debug)]
pub(crate) struct InferenceTables<'db> {
    method_resolutions: ArenaMap<ExprId, (FunctionId, StoredGenericArgs)>,
    field_resolutions: ArenaMap<ExprId, Either<FieldId, TupleFieldId>>,
    variant_resolutions: ExprOrPatMap<VariantId>,
    value_resolutions: ExprOrPatMap<ValueNs>,
    assoc_resolutions: ExprOrPatMap<(CandidateId, StoredGenericArgs)>,
    type_of_expr: ArenaMap<ExprId, StoredTy>,
    type_of_pat: ArenaMap<PatId, StoredTy>,
    type_of_binding: ArenaMap<BindingId, StoredTy>,
    type_of_type_placeholder: ArenaMap<TypeRefId, StoredTy>,
    type_of_opaque: FxHashMap<InternedOpaqueTyId<'db>, StoredTy>,
    has_errors: bool,
    nodes_with_type_mismatches: ExprOrPatSet,
    error_ty: StoredTy,
    expr_adjustments: CsrBuilder<ExprId, Adjustment>,
    assignment_coercions: CsrBuilder<ExprId, Adjustment>,
    pat_adjustments: CsrBuilder<PatId, PatAdjustment>,
    binding_modes: ArenaMap<PatId, BindingMode>,
    skipped_ref_pats: IdxSet<PatId>,
    coercion_casts: IdxSet<ExprId>,
    closures_without_expectation: IdxSet<ExprId>,
    asm_sym_tys: FxHashMap<(ExprId, u32), StoredTy>,
    closures_data: FxHashMap<ExprId, ClosureData>,
}

impl<'db> InferenceTables<'db> {
    fn new(error_ty: Ty<'_>) -> Self {
        InferenceTables {
            method_resolutions: ArenaMap::default(),
            field_resolutions: ArenaMap::default(),
            variant_resolutions: ExprOrPatMap::default(),
            value_resolutions: ExprOrPatMap::default(),
            assoc_resolutions: ExprOrPatMap::default(),
            type_of_expr: ArenaMap::default(),
            type_of_pat: ArenaMap::default(),
            type_of_binding: ArenaMap::default(),
            type_of_type_placeholder: ArenaMap::default(),
            type_of_opaque: FxHashMap::default(),
            has_errors: false,
            nodes_with_type_mismatches: ExprOrPatSet::default(),
            error_ty: error_ty.store(),
            expr_adjustments: CsrBuilder::default(),
            assignment_coercions: CsrBuilder::default(),
            pat_adjustments: CsrBuilder::default(),
            binding_modes: ArenaMap::default(),
            skipped_ref_pats: IdxSet::default(),
            coercion_casts: IdxSet::default(),
            closures_without_expectation: IdxSet::default(),
            asm_sym_tys: FxHashMap::default(),
            closures_data: FxHashMap::default(),
        }
    }

    pub(crate) fn expr_ty(&self, id: ExprId) -> Ty<'db> {
        self.type_of_expr.get(id).map_or(self.error_ty.as_ref(), |it| it.as_ref())
    }

    pub(crate) fn pat_ty(&self, id: PatId) -> Ty<'db> {
        self.type_of_pat.get(id).map_or(self.error_ty.as_ref(), |it| it.as_ref())
    }

    pub(crate) fn binding_ty(&self, id: BindingId) -> Ty<'db> {
        self.type_of_binding.get(id).map_or(self.error_ty.as_ref(), |it| it.as_ref())
    }

    pub(crate) fn type_of_expr_or_pat(&self, id: ExprOrPatId) -> Option<Ty<'db>> {
        match id {
            ExprOrPatId::ExprId(id) => self.type_of_expr.get(id).map(|it| it.as_ref()),
            ExprOrPatId::PatId(id) => self.type_of_pat.get(id).map(|it| it.as_ref()),
        }
    }

    pub(crate) fn type_of_expr_with_adjust(&self, id: ExprId) -> Option<Ty<'db>> {
        match self.expr_adjustments.get(&id).and_then(|adjustments| last_adjustment(adjustments)) {
            Some(adjustment) => Some(adjustment.target.as_ref()),
            None => self.type_of_expr.get(id).map(|it| it.as_ref()),
        }
    }

    pub(crate) fn type_of_pat_with_adjust(&self, id: PatId) -> Ty<'db> {
        match self.pat_adjustments.get(&id).and_then(|adjustments| adjustments.last()) {
            Some(adjusted) => adjusted.source.as_ref(),
            None => self.pat_ty(id),
        }
    }

    pub(crate) fn pat_adjustment(&self, id: PatId) -> Option<&[PatAdjustment]> {
        self.pat_adjustments.get(&id)
    }

    pub(crate) fn expr_adjustment(&self, id: ExprId) -> Option<&[Adjustment]> {
        self.expr_adjustments.get(&id)
    }

    pub(crate) fn binding_mode(&self, id: PatId) -> Option<BindingMode> {
        self.binding_modes.get(id).copied()
    }

    pub(crate) fn is_skipped_ref_pat(&self, pat: PatId) -> bool {
        self.skipped_ref_pats.contains(&pat)
    }

    pub(crate) fn assoc_resolutions_for_expr(
        &self,
        id: ExprId,
    ) -> Option<(CandidateId, GenericArgs<'db>)> {
        self.assoc_resolutions.get(id.into()).map(|(assoc, args)| (*assoc, args.as_ref()))
    }

    pub(crate) fn variant_resolution_for_expr(&self, id: ExprId) -> Option<VariantId> {
        self.variant_resolutions.get(id.into()).copied()
    }

    pub(crate) fn variant_resolution_for_pat(&self, id: PatId) -> Option<VariantId> {
        self.variant_resolutions.get(id.into()).copied()
    }

    /// The result's columns, every type in them resolved.
    fn freeze(
        self,
        tuple_field_access_types: Box<[StoredTys]>,
        diagnostics: Box<[InferenceDiagnostic]>,
        unreachable: Box<[Unreachable]>,
        defined_anon_consts: Box<[AnonConstId<'db>]>,
    ) -> InferenceResult<'db> {
        let InferenceTables {
            method_resolutions,
            field_resolutions,
            variant_resolutions,
            value_resolutions,
            assoc_resolutions,
            type_of_expr,
            type_of_pat,
            type_of_binding,
            type_of_type_placeholder,
            type_of_opaque,
            has_errors,
            mut nodes_with_type_mismatches,
            error_ty,
            expr_adjustments,
            assignment_coercions,
            pat_adjustments,
            binding_modes,
            mut skipped_ref_pats,
            mut coercion_casts,
            mut closures_without_expectation,
            asm_sym_tys,
            closures_data,
        } = self;
        nodes_with_type_mismatches.exprs.shrink_to_fit();
        nodes_with_type_mismatches.pats.shrink_to_fit();
        skipped_ref_pats.shrink_to_fit();
        coercion_casts.shrink_to_fit();
        closures_without_expectation.shrink_to_fit();
        InferenceResult {
            method_resolutions: IndexColumn::from_entries(method_resolutions),
            field_resolutions: FieldColumn::from_entries(field_resolutions),
            variant_resolutions: variant_resolutions.freeze(),
            value_resolutions: value_resolutions.freeze(),
            assoc_resolutions: assoc_resolutions.freeze(),
            tuple_field_access_types,
            type_of_expr: TyColumn::from_entries(type_of_expr),
            type_of_pat: TyColumn::from_entries(type_of_pat),
            type_of_binding: TyColumn::from_entries(type_of_binding),
            type_of_type_placeholder: TyColumn::from_entries(type_of_type_placeholder),
            type_of_opaque: SortedMap::from_entries(type_of_opaque),
            has_errors,
            diagnostics,
            unreachable,
            nodes_with_type_mismatches,
            error_ty,
            expr_adjustments: expr_adjustments.freeze(),
            assignment_coercions: assignment_coercions.freeze(),
            pat_adjustments: pat_adjustments.freeze(),
            binding_modes: ModeColumn::from_entries(binding_modes),
            skipped_ref_pats,
            coercion_casts,
            closures_without_expectation,
            asm_sym_tys: SortedMap::from_entries(asm_sym_tys),
            closures_data: SortedMap::from_entries(closures_data),
            defined_anon_consts,
        }
    }
}

/// The adjustment whose target is an expression's type after adjustment: the last but a
/// `NeverToAny` to `!` (rustc_mir_build's `thir::cx::expr` skips it).
fn last_adjustment(adjustments: &[Adjustment]) -> Option<&Adjustment> {
    // https://github.com/rust-lang/rust/blob/67819923ac8ea353aaa775303f4c3aacbf41d010/compiler/rustc_mir_build/src/thir/cx/expr.rs#L140
    adjustments.iter().rfind(|adj| {
        !matches!(
            adj,
            Adjustment { kind: Adjust::NeverToAny, target } if target.as_ref().is_never()
        )
    })
}

/// Hashes an unordered collection the same for equal values, which may iterate in different
/// orders: each entry by itself, summed.
fn hash_unordered<T: Hash, H: Hasher>(entries: impl ExactSizeIterator<Item = T>, state: &mut H) {
    state.write_usize(entries.len());
    let sum = entries.fold(0u128, |sum, entry| {
        let mut hasher = salsa::StableHasher::new();
        entry.hash(&mut hasher);
        let (low, high) = hasher.finish128();
        sum.wrapping_add(u128::from(high) << 64 | u128::from(low))
    });
    state.write_u128(sum);
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct ClosureData {
    /// Tracks the minimum captures required for a closure;
    /// see `MinCaptureInformationMap` for more details.
    pub min_captures: RootVariableMinCaptureList,

    /// Tracks the fake reads required for a closure and the reason for the fake read.
    /// When performing pattern matching for closures, there are times we don't end up
    /// reading places that are mentioned in a closure (because of _ patterns). However,
    /// to ensure the places are initialized, we introduce fake reads.
    /// Consider these two examples:
    /// ```ignore (discriminant matching with only wildcard arm)
    /// let x: u8;
    /// let c = || match x { _ => () };
    /// ```
    /// In this example, we don't need to actually read/borrow `x` in `c`, and so we don't
    /// want to capture it. However, we do still want an error here, because `x` should have
    /// to be initialized at the point where c is created. Therefore, we add a "fake read"
    /// instead.
    /// ```ignore (destructured assignments)
    /// let c = || {
    ///     let (t1, t2) = t;
    /// }
    /// ```
    /// In the second example, we capture the disjoint fields of `t` (`t.0` & `t.1`), but
    /// we never capture `t`. This becomes an issue when we build MIR as we require
    /// information on `t` in order to create place `t.0` and `t.1`. We can solve this
    /// issue by fake reading `t`.
    pub fake_reads: Box<[(Place, FakeReadCause, SmallVec<[CaptureSourceStack; 2]>)]>,

    /// For each fn, records the "liberated" types of its arguments
    /// and return type. Liberated means that all bound regions
    /// (including late-bound regions) are replaced with free
    /// equivalents. This table is not used in codegen (since regions
    /// are erased there) and hence is not serialized to metadata.
    ///
    /// This table also contains the "revealed" values for any `impl Trait`
    /// that appear in the signature and whose values are being inferred
    /// by this function.
    ///
    /// # Example
    ///
    /// ```rust
    /// # use std::fmt::Debug;
    /// fn foo(x: &u32) -> impl Debug { *x }
    /// ```
    ///
    /// The function signature here would be:
    ///
    /// ```ignore (illustrative)
    /// for<'a> fn(&'a u32) -> Foo
    /// ```
    ///
    /// where `Foo` is an opaque type created for this function.
    ///
    ///
    /// The *liberated* form of this would be
    ///
    /// ```ignore (illustrative)
    /// fn(&'a u32) -> u32
    /// ```
    ///
    /// Note that `'a` is not bound (it would be an `ReLateParam`) and
    /// that the `Foo` opaque type is replaced by its hidden type.
    pub liberated_sig: StoredFnSig,

    /// rustc's `closure_kind_origins`: for a closure whose kind was inferred `FnMut` or
    /// `FnOnce`, the use that made it so and the place it captured for it.
    pub kind_origin: Option<(ExprOrPatIdPacked, Place)>,
}

impl Hash for ClosureData {
    fn hash<H: Hasher>(&self, state: &mut H) {
        let ClosureData { min_captures, fake_reads, liberated_sig, kind_origin } = self;
        hash_unordered(min_captures.iter(), state);
        fake_reads.hash(state);
        liberated_sig.hash(state);
        kind_origin.hash(state);
    }
}

/// Part of `MinCaptureInformationMap`; Maps a root variable to the list of `CapturedPlace`.
/// Used to track the minimum set of `Place`s that need to be captured to support all
/// Places captured by the closure starting at a given root variable.
///
/// This provides a convenient and quick way of checking if a variable being used within
/// a closure is a capture of a local variable.
pub(crate) type RootVariableMinCaptureList = FxIndexMap<BindingId, MinCaptureList>;

/// Part of `MinCaptureInformationMap`; List of `CapturePlace`s.
pub(crate) type MinCaptureList = Vec<CapturedPlace>;

/// A composite describing a `Place` that is captured by a closure.
#[derive(Eq, PartialEq, Clone, Debug, Hash)]
pub struct CapturedPlace {
    /// The `Place` that is captured.
    pub place: Place,

    /// `CaptureKind` and expression(s) that resulted in such capture of `place`.
    pub info: CaptureInfo,

    /// Represents if `place` can be mutated or not.
    pub mutability: Mutability,
}

impl CapturedPlace {
    pub fn is_by_ref(&self) -> bool {
        match self.info.capture_kind {
            UpvarCapture::ByValue | UpvarCapture::ByUse => false,
            UpvarCapture::ByRef(..) => true,
        }
    }

    pub fn captured_local(&self) -> BindingId {
        match self.place.base {
            PlaceBase::Upvar { var_id: local, .. } | PlaceBase::Local(local) => local,
            PlaceBase::Rvalue | PlaceBase::StaticItem => {
                unreachable!("only locals can be captured")
            }
        }
    }

    /// The type of the capture stored in the closure, which is different from the type of the captured place
    /// if we capture by reference.
    pub fn captured_ty<'db>(&self, db: &'db Db) -> Ty<'db> {
        let place_ty = self.place.ty();
        let make_ref = |mutbl| {
            let interner = DbInterner::new_no_crate(db);
            let region = Region::new_erased(interner);
            Ty::new_ref(interner, region, place_ty, mutbl)
        };
        match self.info.capture_kind {
            UpvarCapture::ByUse | UpvarCapture::ByValue => place_ty,
            UpvarCapture::ByRef(kind) => make_ref(kind.to_mutbl_lossy()),
        }
    }
}

#[derive(Clone)]
pub struct CaptureSourceStack(CaptureSourceStackRepr);

#[derive(Clone)]
enum CaptureSourceStackRepr {
    One(ExprOrPatIdPacked),
    Two([ExprOrPatIdPacked; 2]),
    Many(ThinVec<ExprOrPatIdPacked>),
}

impl PartialEq for CaptureSourceStack {
    fn eq(&self, other: &Self) -> bool {
        **self == **other
    }
}

impl Eq for CaptureSourceStack {}

impl std::hash::Hash for CaptureSourceStack {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        (**self).hash(state);
    }
}

#[cfg(target_pointer_width = "64")]
const _: () = assert!(size_of::<CaptureSourceStack>() == 16);

impl Deref for CaptureSourceStack {
    type Target = [ExprOrPatIdPacked];

    #[inline]
    fn deref(&self) -> &Self::Target {
        match &self.0 {
            CaptureSourceStackRepr::One(it) => std::slice::from_ref(it),
            CaptureSourceStackRepr::Two(it) => it,
            CaptureSourceStackRepr::Many(it) => it,
        }
    }
}

impl fmt::Debug for CaptureSourceStack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_tuple("CaptureSourceStack").field(&&**self).finish()
    }
}

impl CaptureSourceStack {
    #[inline]
    pub fn len(&self) -> usize {
        match &self.0 {
            CaptureSourceStackRepr::One(_) => 1,
            CaptureSourceStackRepr::Two(_) => 2,
            CaptureSourceStackRepr::Many(it) => it.len(),
        }
    }

    #[inline]
    pub(crate) fn from_single(id: ExprOrPatIdPacked) -> Self {
        Self(CaptureSourceStackRepr::One(id))
    }

    #[inline]
    pub fn final_source(&self) -> ExprOrPatIdPacked {
        *self.last().expect("should always have a final source")
    }

    pub fn push(&mut self, new_id: ExprOrPatIdPacked) {
        match &mut self.0 {
            CaptureSourceStackRepr::One(old_id) => {
                self.0 = CaptureSourceStackRepr::Two([*old_id, new_id])
            }
            CaptureSourceStackRepr::Two([old_id1, old_id2]) => {
                self.0 = CaptureSourceStackRepr::Many(ThinVec::from([*old_id1, *old_id2, new_id]));
            }
            CaptureSourceStackRepr::Many(old_ids) => old_ids.push(new_id),
        }
    }

    pub fn truncate(&mut self, new_len: usize) {
        debug_assert!(new_len > 0);
        match &mut self.0 {
            CaptureSourceStackRepr::One(_) => {}
            CaptureSourceStackRepr::Two([first, _]) => {
                if new_len == 1 {
                    self.0 = CaptureSourceStackRepr::One(*first)
                }
            }
            CaptureSourceStackRepr::Many(ids) => ids.truncate(new_len),
        }
    }

    pub fn shrink_to_fit(&mut self) {
        match &mut self.0 {
            CaptureSourceStackRepr::One(_) | CaptureSourceStackRepr::Two(_) => {}
            CaptureSourceStackRepr::Many(ids) => match **ids {
                [one] => self.0 = CaptureSourceStackRepr::One(one),
                [first, second] => self.0 = CaptureSourceStackRepr::Two([first, second]),
                _ => ids.shrink_to_fit(),
            },
        }
    }
}

/// Part of `MinCaptureInformationMap`; describes the capture kind (&, &mut, move)
/// for a particular capture as well as identifying the part of the source code
/// that triggered this capture to occur.
#[derive(Eq, PartialEq, Clone, Debug, Hash)]
pub struct CaptureInfo {
    pub sources: SmallVec<[CaptureSourceStack; 2]>,

    /// Capture mode that was selected
    pub capture_kind: UpvarCapture,
}

/// Information describing the capture of an upvar. This is computed
/// during `typeck`, specifically by `regionck`.
#[derive(Eq, PartialEq, Clone, Debug, Copy, Hash)]
pub enum UpvarCapture {
    /// Upvar is captured by reference.
    ByRef(BorrowKind),

    /// Upvar is captured by use. This is true when the closure is labeled `use`.
    ByUse,

    /// Upvar is captured by value. This is always true when the
    /// closure is labeled `move`, but can also be true in other cases
    /// depending on inference.
    ByValue,
}

// Used in closure::analysis::determine_capture_info
impl PartialOrd for UpvarCapture {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<cmp::Ordering> {
        match (self, other) {
            (Self::ByValue, Self::ByValue) | (Self::ByUse, Self::ByUse) => {
                Some(cmp::Ordering::Equal)
            }
            (Self::ByValue | Self::ByUse, Self::ByRef(_)) => Some(cmp::Ordering::Greater),
            (Self::ByRef(_), Self::ByValue | Self::ByUse) => Some(cmp::Ordering::Less),
            (Self::ByRef(left), Self::ByRef(right)) => Some(left.cmp(right)),
            (Self::ByUse, Self::ByValue) | (Self::ByValue, Self::ByUse) => None,
        }
    }
}

/// The inference of a body. Free functions rather than methods of a tracked impl, so that
/// [`crate::evict`] can name them to drop their values.
#[salsa::tracked(returns(ref), cycle_result = infer_cycle_result, lru = 0)]
pub(crate) fn infer_body_query<'db>(
    db: &'db Db,
    def: DefWithBodyId,
) -> InferenceResult<'db> {
    infer_query(db, def)
}

/// Infer types for all const expressions in an item's signature.
///
/// Returns an `InferenceResult` containing type information for array lengths,
/// const generic arguments, and other const expressions appearing in type
/// positions within the item's signature.
#[salsa::tracked(returns(ref), cycle_result = infer_anon_const_cycle_result, lru = 0)]
pub(crate) fn infer_anon_const_body_query<'db>(
    db: &'db Db,
    def: AnonConstId<'db>,
) -> InferenceResult<'db> {
    infer_anon_const_query(db, def)
}

impl<'db> InferenceResult<'db> {
    #[inline]
    pub fn of(
        db: &'db Db,
        def: impl Into<InferBodyId<'db>>,
    ) -> &'db InferenceResult<'db> {
        match def.into() {
            InferBodyId::DefWithBodyId(it) => infer_body_query(db, it),
            InferBodyId::AnonConstId(it) => {
                // A `const { .. }` block of a body, which MIR lowering makes a constant of its
                // own, was inferred with the body, as rustc infers inline consts.
                if let ExpressionStoreOwnerId::Body(body) = it.loc(db).owner
                    && it.is_inline_const(db)
                {
                    return infer_body_query(db, body);
                }
                infer_anon_const_body_query(db, it)
            }
        }
    }
}

impl<'db> InferenceResult<'db> {
    fn new(error_ty: Ty<'_>) -> Self {
        Self {
            method_resolutions: Default::default(),
            field_resolutions: Default::default(),
            variant_resolutions: Default::default(),
            value_resolutions: Default::default(),
            assoc_resolutions: Default::default(),
            tuple_field_access_types: Default::default(),
            diagnostics: Default::default(),
            unreachable: Default::default(),
            nodes_with_type_mismatches: Default::default(),
            type_of_expr: Default::default(),
            type_of_pat: Default::default(),
            type_of_binding: Default::default(),
            type_of_type_placeholder: Default::default(),
            type_of_opaque: Default::default(),
            skipped_ref_pats: Default::default(),
            has_errors: Default::default(),
            error_ty: error_ty.store(),
            pat_adjustments: Default::default(),
            binding_modes: Default::default(),
            expr_adjustments: Default::default(),
            assignment_coercions: Default::default(),
            coercion_casts: Default::default(),
            closures_without_expectation: Default::default(),
            asm_sym_tys: Default::default(),
            closures_data: Default::default(),
            defined_anon_consts: Default::default(),
        }
    }

    pub fn method_resolution(&self, expr: ExprId) -> Option<(FunctionId, GenericArgs<'db>)> {
        self.method_resolutions.get(expr).map(|(func, args)| (*func, args.as_ref()))
    }
    pub fn field_resolution(&self, expr: ExprId) -> Option<Either<FieldId, TupleFieldId>> {
        self.field_resolutions.get(expr)
    }
    pub fn variant_resolution_for_expr(&self, id: ExprId) -> Option<VariantId> {
        self.variant_resolutions.get(id.into()).copied()
    }
    pub fn variant_resolution_for_pat(&self, id: PatId) -> Option<VariantId> {
        self.variant_resolutions.get(id.into()).copied()
    }
    pub fn variant_resolution_for_expr_or_pat(&self, id: ExprOrPatId) -> Option<VariantId> {
        match id {
            ExprOrPatId::ExprId(id) => self.variant_resolution_for_expr(id),
            ExprOrPatId::PatId(id) => self.variant_resolution_for_pat(id),
        }
    }
    /// What the value path `id` (a path expression or pattern) resolved to.
    pub fn value_resolution(&self, id: ExprOrPatId) -> Option<ValueNs> {
        let node = match id {
            ExprOrPatId::ExprId(id) => id.into(),
            ExprOrPatId::PatId(id) => id.into(),
        };
        self.value_resolutions.get(node).copied()
    }
    pub fn assoc_resolutions_for_expr<'a>(
        &self,
        id: ExprId,
    ) -> Option<(CandidateId, GenericArgs<'a>)> {
        self.assoc_resolutions.get(id.into()).map(|(assoc, args)| (*assoc, args.as_ref()))
    }
    pub fn assoc_resolutions_for_pat<'a>(
        &self,
        id: PatId,
    ) -> Option<(CandidateId, GenericArgs<'a>)> {
        self.assoc_resolutions.get(id.into()).map(|(assoc, args)| (*assoc, args.as_ref()))
    }
    pub fn assoc_resolutions_for_expr_or_pat<'a>(
        &self,
        id: ExprOrPatId,
    ) -> Option<(CandidateId, GenericArgs<'a>)> {
        match id {
            ExprOrPatId::ExprId(id) => self.assoc_resolutions_for_expr(id),
            ExprOrPatId::PatId(id) => self.assoc_resolutions_for_pat(id),
        }
    }
    pub fn expr_or_pat_has_type_mismatch(&self, node: ExprOrPatIdPacked) -> bool {
        self.nodes_with_type_mismatches.contains(node)
    }
    pub fn expr_has_type_mismatch(&self, expr: ExprId) -> bool {
        self.expr_or_pat_has_type_mismatch(expr.into())
    }
    pub fn pat_has_type_mismatch(&self, pat: PatId) -> bool {
        self.expr_or_pat_has_type_mismatch(pat.into())
    }
    pub fn exprs_have_type_mismatches(&self) -> bool {
        !self.nodes_with_type_mismatches.exprs.is_empty()
    }
    pub fn has_type_mismatches(&self) -> bool {
        !self.nodes_with_type_mismatches.is_empty()
    }
    pub fn placeholder_types<'a>(&self) -> impl Iterator<Item = (TypeRefId, Ty<'a>)> {
        self.type_of_type_placeholder.iter().map(|(type_ref, ty)| (type_ref, ty.as_ref()))
    }
    pub fn type_of_type_placeholder<'a>(&self, type_ref: TypeRefId) -> Option<Ty<'a>> {
        self.type_of_type_placeholder.get(type_ref).map(|ty| ty.as_ref())
    }
    pub fn type_of_expr_or_pat<'a>(&self, id: ExprOrPatId) -> Option<Ty<'a>> {
        match id {
            ExprOrPatId::ExprId(id) => self.type_of_expr.get(id).map(|it| it.as_ref()),
            ExprOrPatId::PatId(id) => self.type_of_pat.get(id).map(|it| it.as_ref()),
        }
    }
    pub fn type_of_expr_with_adjust<'a>(&self, id: ExprId) -> Option<Ty<'a>> {
        match self.expr_adjustments.get(&id).and_then(|adjustments| last_adjustment(adjustments)) {
            Some(adjustment) => Some(adjustment.target.as_ref()),
            None => self.type_of_expr.get(id).map(|it| it.as_ref()),
        }
    }
    pub fn type_of_pat_with_adjust<'a>(&self, id: PatId) -> Ty<'a> {
        match self.pat_adjustments.get(&id).and_then(|adjustments| adjustments.last()) {
            Some(adjusted) => adjusted.source.as_ref(),
            None => self.pat_ty(id),
        }
    }
    pub fn is_erroneous(&self) -> bool {
        self.has_errors && self.type_of_expr.is_empty()
    }

    pub fn diagnostics(&self) -> &[InferenceDiagnostic] {
        &self.diagnostics
    }

    /// In the order type checking warns of them.
    pub fn unreachable(&self) -> &[Unreachable] {
        &self.unreachable
    }

    /// The anonymous consts of the body this inferred, including those inside them.
    pub fn defined_anon_consts(&self) -> &[AnonConstId<'db>] {
        &self.defined_anon_consts
    }

    pub fn tuple_field_access_type<'a>(&self, id: TupleId) -> Tys<'a> {
        self.tuple_field_access_types[id.0 as usize].as_ref()
    }

    pub fn pat_adjustment(&self, id: PatId) -> Option<&[PatAdjustment]> {
        self.pat_adjustments.get(&id)
    }

    pub fn expr_adjustment(&self, id: ExprId) -> Option<&[Adjustment]> {
        self.expr_adjustments.get(&id)
    }

    pub fn assignment_coercion(&self, place: ExprId) -> &[Adjustment] {
        self.assignment_coercions.get(&place).unwrap_or_default()
    }

    pub fn binding_mode(&self, id: PatId) -> Option<BindingMode> {
        self.binding_modes.get(id).copied()
    }

    // This method is consumed by external tools to run rust-analyzer as a library. Don't remove, please.
    pub fn expression_types<'a>(&self) -> impl Iterator<Item = (ExprId, Ty<'a>)> {
        self.type_of_expr.iter().map(|(k, v)| (k, v.as_ref()))
    }

    // This method is consumed by external tools to run rust-analyzer as a library. Don't remove, please.
    pub fn pattern_types<'a>(&self) -> impl Iterator<Item = (PatId, Ty<'a>)> {
        self.type_of_pat.iter().map(|(k, v)| (k, v.as_ref()))
    }

    // This method is consumed by external tools to run rust-analyzer as a library. Don't remove, please.
    pub fn binding_types<'a>(&self) -> impl Iterator<Item = (BindingId, Ty<'a>)> {
        self.type_of_binding.iter().map(|(k, v)| (k, v.as_ref()))
    }

    // This method is consumed by external tools to run rust-analyzer as a library. Don't remove, please.
    pub fn return_position_impl_trait_types<'a>(
        &'a self,
        db: &'a Db,
    ) -> impl Iterator<Item = (ImplTraitIdx, Ty<'a>)> {
        self.type_of_opaque.iter().filter_map(move |(id, ty)| {
            let ImplTraitId::ReturnTypeImplTrait(_, rpit_idx) = id.loc(db) else {
                return None;
            };
            Some((rpit_idx, ty.as_ref()))
        })
    }

    pub fn expr_ty<'a>(&self, id: ExprId) -> Ty<'a> {
        self.type_of_expr.get(id).map_or(self.error_ty.as_ref(), |it| it.as_ref())
    }

    pub fn pat_ty<'a>(&self, id: PatId) -> Ty<'a> {
        self.type_of_pat.get(id).map_or(self.error_ty.as_ref(), |it| it.as_ref())
    }

    pub fn expr_or_pat_ty<'a>(&self, id: ExprOrPatId) -> Ty<'a> {
        self.type_of_expr_or_pat(id).unwrap_or(self.error_ty.as_ref())
    }

    pub fn binding_ty<'a>(&self, id: BindingId) -> Ty<'a> {
        self.type_of_binding.get(id).map_or(self.error_ty.as_ref(), |it| it.as_ref())
    }

    /// This does not deduplicate, which means you'll get the types once per capture.
    pub fn closure_captures_tys<'a>(&self, closure: ExprId) -> impl Iterator<Item = Ty<'a>> {
        self.closures_data[&closure]
            .min_captures
            .values()
            .flat_map(|captures| captures.iter().map(|capture| capture.place.ty()))
    }

    /// Like [`Self::closure_captures_tys()`], but using [`CapturedPlace::captured_ty()`].
    pub fn closure_captures_captured_tys<'a>(
        &self,
        db: &'a Db,
        closure: ExprId,
    ) -> impl Iterator<Item = Ty<'a>> {
        self.closures_data[&closure]
            .min_captures
            .values()
            .flat_map(|captures| captures.iter().map(|capture| capture.captured_ty(db)))
    }

    pub fn is_skipped_ref_pat(&self, pat: PatId) -> bool {
        self.skipped_ref_pats.contains(&pat)
    }
}

#[derive(Debug, Clone, Copy)]
enum DerefPatBorrowMode {
    Borrow(Mutability),
    Box,
}

/// The inference context contains all information needed during type inference.
#[derive(Debug)]
pub(crate) struct InferenceContext<'db> {
    pub(crate) db: &'db Db,
    pub(crate) owner: InferBodyId<'db>,
    pub(crate) store_owner: ExpressionStoreOwnerId,
    pub(crate) generic_def: GenericDefId,
    pub(crate) store: &'db ExpressionStore,
    pub(crate) lowering_mode: LoweringMode,
    /// Generally you should not resolve things via this resolver. Instead create a TyLoweringContext
    /// and resolve the path via its methods. This will ensure proper error reporting.
    pub(crate) resolver: Resolver<'db>,
    target_features: OnceCell<(TargetFeatures<'db>, TargetFeatureIsSafeInTarget)>,
    pub(crate) edition: Edition,
    allow_using_generic_params: bool,
    generics: OnceCell<Generics<'db>>,
    identity_args: OnceCell<GenericArgs<'db>>,
    pub(crate) table: unify::InferenceTable<'db>,
    pub(crate) lang_items: &'db LangItems,
    pub(crate) features: &'db UnstableFeatures,
    /// The traits in scope, disregarding block modules. This is used for caching purposes.
    traits_in_scope: FxHashSet<TraitId>,
    /// The traits in scope in the innermost block module that adds some: which module, whether
    /// it adds any, and `traits_in_scope` together with the block's.
    block_traits: RefCell<(Option<hir_def::ModuleId>, bool, std::rc::Rc<FxHashSet<TraitId>>)>,
    pub(crate) result: InferenceTables<'db>,
    tuple_field_accesses_rev:
        IndexSet<Tys<'db>, std::hash::BuildHasherDefault<rustc_hash::FxHasher>>,
    /// The return type of the function being inferred, the closure or async block if we're
    /// currently within one.
    ///
    /// We might consider using a nested inference context for checking
    /// closures so we can swap all shared things out at once.
    return_ty: Ty<'db>,
    /// If `Some`, this stores coercion information for returned
    /// expressions. If `None`, this is in a context where return is
    /// inappropriate, such as a const expression.
    return_coercion: Option<DynamicCoerceMany<'db>>,
    /// The resume type and the yield type, respectively, of the coroutine being inferred.
    resume_yield_tys: Option<(Ty<'db>, Ty<'db>)>,
    /// The `async`/`gen` desugaring of the coroutine being inferred, `None` outside one or in a
    /// `#[coroutine]` closure: a `yield` in an `async` body is E0727.
    coroutine_desugaring: Option<hir_def::hir::CoroutineKind>,
    diverges: Diverges,
    /// What [`InferenceContext::warn_if_unreachable`] found, empty (and unallocated) unless
    /// something diverged before more code.
    unreachable: Vec<Unreachable>,
    breakables: Vec<BreakableContext<'db>>,
    types: &'db crate::next_solver::DefaultAny<'db>,

    deferred_cast_checks: Vec<CastCheck<'db>>,

    /// Each call of a closure whose kind is not known yet, by the expression defining the
    /// closure or coroutine closure, in the order they were recorded.
    deferred_call_resolutions: Vec<(ExprId, DeferredCallResolution<'db>)>,

    diagnostics: Diagnostics,
    vars_emitted_type_must_be_known_for: FxHashSet<Term<'db>>,

    defined_anon_consts: RefCell<ThinVec<AnonConstId<'db>>>,
}

#[derive(Clone, Debug)]
struct BreakableContext<'db> {
    /// Whether this context contains at least one break expression.
    may_break: bool,
    /// The coercion target of the context.
    coerce: Option<DynamicCoerceMany<'db>>,
    /// The optional label of the context.
    label: Option<LabelId>,
    kind: BreakableKind,
}

#[derive(Clone, Debug)]
enum BreakableKind {
    Block,
    Loop,
    /// A border is something like an async block, closure etc. Anything that prevents
    /// breaking/continuing through
    Border,
}

fn find_breakable(ctxs: &[BreakableContext<'_>], label: Option<LabelId>) -> Option<usize> {
    let mut ctxs = ctxs
        .iter()
        .enumerate()
        .rev()
        .take_while(|(_, it)| matches!(it.kind, BreakableKind::Block | BreakableKind::Loop));
    let result = match label {
        Some(_) => ctxs.find(|(_, ctx)| ctx.label == label),
        None => ctxs.find(|(_, ctx)| matches!(ctx.kind, BreakableKind::Loop)),
    };
    result.map(|(idx, _)| idx)
}

fn find_continuable(ctxs: &[BreakableContext<'_>], label: Option<LabelId>) -> Option<usize> {
    find_breakable(ctxs, label)
        .filter(|&idx| label.is_none() || matches!(ctxs[idx].kind, BreakableKind::Loop))
}

impl<'db> InferenceContext<'db> {
    fn new(
        db: &'db Db,
        owner: InferBodyId<'db>,
        store_owner: ExpressionStoreOwnerId,
        generic_def: GenericDefId,
        store: &'db ExpressionStore,
        resolver: Resolver<'db>,
        allow_using_generic_params: bool,
        lowering_mode: LoweringMode,
    ) -> Self {
        let trait_env = db.trait_environment(generic_def);
        let table = unify::InferenceTable::new(db, trait_env, resolver.krate(), owner);
        let types = crate::next_solver::default_types();
        InferenceContext {
            result: InferenceTables::new(types.types.error),
            return_ty: types.types.error, // set in collect_* calls
            types,
            target_features: OnceCell::new(),
            lang_items: table.interner().lang_items(),
            features: resolver.top_level_def_map().features(),
            edition: resolver.krate().data(db).edition,
            table,
            tuple_field_accesses_rev: Default::default(),
            resume_yield_tys: None,
            coroutine_desugaring: None,
            return_coercion: None,
            db,
            owner,
            store_owner,
            generic_def,
            allow_using_generic_params,
            generics: OnceCell::new(),
            identity_args: OnceCell::new(),
            store,
            traits_in_scope: resolver.traits_in_scope(db),
            block_traits: RefCell::new((None, false, std::rc::Rc::default())),
            resolver,
            diverges: Diverges::Maybe,
            unreachable: Vec::new(),
            breakables: Vec::new(),
            deferred_cast_checks: Vec::new(),
            diagnostics: Diagnostics::default(),
            vars_emitted_type_must_be_known_for: FxHashSet::default(),
            deferred_call_resolutions: Vec::new(),
            defined_anon_consts: RefCell::new(ThinVec::new()),
            lowering_mode,
        }
    }

    fn merge(&mut self, other: &InferenceResult<'db>) {
        let InferenceTables {
            method_resolutions,
            field_resolutions,
            variant_resolutions,
            value_resolutions,
            assoc_resolutions,
            type_of_expr,
            type_of_pat,
            type_of_binding,
            type_of_type_placeholder,
            type_of_opaque,
            has_errors: _,
            nodes_with_type_mismatches,
            error_ty: _,
            expr_adjustments,
            assignment_coercions,
            pat_adjustments,
            binding_modes,
            skipped_ref_pats,
            coercion_casts,
            closures_without_expectation,
            asm_sym_tys,
            closures_data,
        } = &mut self.result;
        merge_index(method_resolutions, other.method_resolutions.iter());
        for (node, variant) in other.variant_resolutions.iter() {
            debug_assert!(variant_resolutions.get(node).is_none_or(|it| it == variant));
            variant_resolutions.insert(node, *variant);
        }
        for (node, value) in other.value_resolutions.iter() {
            debug_assert!(value_resolutions.get(node).is_none_or(|it| it == value));
            value_resolutions.insert(node, *value);
        }
        for (node, resolution) in other.assoc_resolutions.iter() {
            debug_assert!(assoc_resolutions.get(node).is_none_or(|it| it == resolution));
            assoc_resolutions.insert(node, resolution.clone());
        }
        for (field_expr, mut field_resolution) in other.field_resolutions.iter() {
            if let Either::Right(tuple_field) = &mut field_resolution {
                let tys = other.tuple_field_access_type(tuple_field.tuple);
                tuple_field.tuple =
                    TupleId(self.tuple_field_accesses_rev.insert_full(tys).0 as u32);
            }
            field_resolutions.insert(field_expr, field_resolution);
        }
        merge_index(type_of_expr, other.type_of_expr.iter());
        merge_index(type_of_pat, other.type_of_pat.iter());
        merge_index(type_of_binding, other.type_of_binding.iter());
        merge_index(type_of_type_placeholder, other.type_of_type_placeholder.iter());
        merge_map(type_of_opaque, other.type_of_opaque.iter());
        merge_csr(expr_adjustments, other.expr_adjustments.iter());
        merge_csr(assignment_coercions, other.assignment_coercions.iter());
        merge_csr(pat_adjustments, other.pat_adjustments.iter());
        merge_index(binding_modes, other.binding_modes.iter());
        skipped_ref_pats.extend(other.skipped_ref_pats.iter());
        coercion_casts.extend(other.coercion_casts.iter());
        closures_without_expectation.extend(other.closures_without_expectation.iter());
        merge_map(asm_sym_tys, other.asm_sym_tys.iter());
        merge_map(closures_data, other.closures_data.iter());
        for node in other.nodes_with_type_mismatches.iter() {
            nodes_with_type_mismatches.insert(node);
        }
        self.defined_anon_consts.borrow_mut().extend(other.defined_anon_consts.iter().copied());
        self.diagnostics.extend(&other.diagnostics);
        self.unreachable.extend(other.unreachable.iter().cloned());

        #[cfg_attr(debug_assertions, track_caller)]
        fn merge_index<'a, K, V: Clone + PartialEq + 'a>(
            dest: &mut ArenaMap<la_arena::Idx<K>, V>,
            source: impl Iterator<Item = (la_arena::Idx<K>, &'a V)>,
        ) {
            for (key, value) in source {
                debug_assert!(dest.get(key).is_none_or(|it| it == value));
                dest.insert(key, value.clone());
            }
        }

        #[cfg_attr(debug_assertions, track_caller)]
        fn merge_map<'a, K: Hash + Eq + Clone + 'a, V: Clone + PartialEq + 'a>(
            dest: &mut FxHashMap<K, V>,
            source: impl Iterator<Item = (&'a K, &'a V)>,
        ) {
            for (key, value) in source {
                debug_assert!(dest.get(key).is_none_or(|it| it == value));
                dest.insert(key.clone(), value.clone());
            }
        }

        #[cfg_attr(debug_assertions, track_caller)]
        fn merge_csr<'a, K: columns::DenseKey, T: Clone + PartialEq + 'a>(
            dest: &mut CsrBuilder<K, T>,
            source: impl Iterator<Item = (K, &'a [T])>,
        ) {
            for (key, items) in source {
                debug_assert!(dest.get(&key).is_none_or(|it| **it == *items));
                dest.insert(key, items.iter().cloned());
            }
        }
    }

    #[inline]
    fn krate(&self) -> Crate {
        self.resolver.krate()
    }

    fn target_features(&self) -> (&TargetFeatures<'db>, TargetFeatureIsSafeInTarget) {
        let (target_features, target_feature_is_safe) = self.target_features.get_or_init(|| {
            let target_features = match self.store_owner {
                ExpressionStoreOwnerId::Body(DefWithBodyId::FunctionId(id)) => {
                    TargetFeatures::from_fn(self.db, id)
                }
                _ => TargetFeatures::default(),
            };
            let target_feature_is_safe = match &self.krate().workspace_data(self.db).target {
                Ok(target) => crate::utils::target_feature_is_safe_in_target(target),
                Err(_) => TargetFeatureIsSafeInTarget::No,
            };
            (target_features, target_feature_is_safe)
        });
        (target_features, *target_feature_is_safe)
    }

    /// How should a deref pattern find the place for its inner pattern to match on?
    ///
    /// In most cases, if the pattern recursively contains a `ref mut` binding, we find the inner
    /// pattern's scrutinee by calling `DerefMut::deref_mut`, and otherwise we call `Deref::deref`.
    /// However, for boxes we can use a built-in deref instead, which doesn't borrow the scrutinee;
    /// in this case, we return `DerefPatBorrowMode::Box`.
    fn deref_pat_borrow_mode(&self, pointer_ty: Ty<'_>, inner: PatId) -> DerefPatBorrowMode {
        if pointer_ty.is_box() {
            DerefPatBorrowMode::Box
        } else {
            let mutability =
                if self.pat_has_ref_mut_binding(inner) { Mutability::Mut } else { Mutability::Not };
            DerefPatBorrowMode::Borrow(mutability)
        }
    }

    #[inline]
    fn set_tainted_by_errors(&mut self) {
        self.result.has_errors = true;
    }

    /// Copy the inference of defined anon consts to ourselves, so that we don't need to lookup the defining
    /// anon const when looking the type of something.
    fn merge_anon_consts(&mut self) {
        let mut defined_anon_consts = std::mem::take(&mut *self.defined_anon_consts.borrow_mut());
        defined_anon_consts.retain(|&konst| {
            if konst.loc(self.db).owner != self.store_owner {
                // This comes from the signature, we don't define it.
                return false;
            }

            let const_infer = InferenceResult::of(self.db, konst);
            self.merge(const_infer);
            true
        });
        // Caution, other defined anon consts might have been added by `merge()`!
        self.defined_anon_consts.borrow_mut().append(&mut defined_anon_consts);
    }

    fn resolve_all(self) -> InferenceResult<'db> {
        let InferenceContext {
            table,
            mut result,
            tuple_field_accesses_rev,
            diagnostics,
            types,
            vars_emitted_type_must_be_known_for,
            defined_anon_consts,
            mut unreachable,
            db,
            owner,
            ..
        } = self;
        let diagnostics = diagnostics.finish();
        if !unreachable.is_empty() && in_automatically_derived_impl(db, owner) {
            unreachable = Vec::new();
        }
        // Destructure every single field so whenever new fields are added to `InferenceTables` we
        // don't forget to handle them here.
        let InferenceTables {
            method_resolutions,
            field_resolutions: _,
            variant_resolutions: _,
            value_resolutions: _,
            assoc_resolutions,
            type_of_expr,
            type_of_pat,
            type_of_binding,
            type_of_type_placeholder,
            type_of_opaque: _,
            skipped_ref_pats: _,
            closures_data,
            has_errors,
            error_ty: _,
            pat_adjustments,
            binding_modes: _,
            expr_adjustments,
            assignment_coercions,
            coercion_casts: _,
            closures_without_expectation: _,
            asm_sym_tys,
            nodes_with_type_mismatches,
        } = &mut result;

        let defined_anon_consts = defined_anon_consts.into_inner();

        let mut resolver =
            WriteBackCtxt::new(table, diagnostics, vars_emitted_type_must_be_known_for);

        for ty in type_of_expr.values_mut() {
            resolver.resolve_completely(ty);
        }
        for ty in type_of_pat.values_mut() {
            resolver.resolve_completely(ty);
        }
        for ty in type_of_binding.values_mut() {
            resolver.resolve_completely(ty);
        }
        for ty in type_of_type_placeholder.values_mut() {
            resolver.resolve_completely(ty);
        }
        for ty in asm_sym_tys.values_mut() {
            resolver.resolve_completely(ty);
        }

        if !nodes_with_type_mismatches.is_empty() {
            *has_errors = true;
        }
        for (_, subst) in method_resolutions.values_mut() {
            resolver.resolve_completely(subst);
        }
        for (_, subst) in assoc_resolutions.values_mut() {
            resolver.resolve_completely(subst);
        }
        for adjustment in expr_adjustments.values_mut().flatten() {
            resolver.resolve_completely(&mut adjustment.target);
        }
        for adjustment in assignment_coercions.values_mut().flatten() {
            resolver.resolve_completely(&mut adjustment.target);
        }
        for adjustment in pat_adjustments.values_mut().flatten() {
            resolver.resolve_completely(&mut adjustment.source);
        }
        for closure_data in closures_data.values_mut() {
            let ClosureData { min_captures, fake_reads, liberated_sig, kind_origin } = closure_data;
            let dummy_place = || Place {
                base_ty: types.types.error.store(),
                base: closure::analysis::expr_use_visitor::PlaceBase::Rvalue,
                projections: Vec::new(),
            };
            if let Some((_, place)) = kind_origin {
                resolver.resolve_completely_with_default(place, dummy_place());
                place.projections.shrink_to_fit();
            }

            for (place, _, sources) in fake_reads {
                resolver.resolve_completely_with_default(place, dummy_place());
                place.projections.shrink_to_fit();
                for source in &mut *sources {
                    source.shrink_to_fit();
                }
                sources.shrink_to_fit();
            }

            for min_capture in min_captures.values_mut() {
                for captured in &mut *min_capture {
                    let CapturedPlace { place, info, mutability: _ } = captured;
                    resolver.resolve_completely_with_default(place, dummy_place());
                    let CaptureInfo { sources, capture_kind: _ } = info;
                    for source in &mut *sources {
                        source.shrink_to_fit();
                    }
                    sources.shrink_to_fit();
                }
                min_capture.shrink_to_fit();
            }
            min_captures.shrink_to_fit();

            resolver.resolve_completely(liberated_sig);
        }
        let tuple_field_access_types: Box<[StoredTys]> = tuple_field_accesses_rev
            .into_iter()
            .map(|mut subst| {
                resolver.resolve_completely(&mut subst);
                subst.store()
            })
            .collect();

        let (diagnostics, resolver_has_errors) = resolver.resolve_diagnostics();
        *has_errors |= resolver_has_errors;

        result.freeze(
            tuple_field_access_types,
            diagnostics.into_iter().collect(),
            unreachable.into_boxed_slice(),
            defined_anon_consts.into_iter().collect(),
        )
    }

    fn collect_const_or_static(&mut self, id: ValueTyDefId) {
        let return_ty = self.db.value_ty(id).unwrap().instantiate_identity().skip_norm_wip();
        self.return_ty = self.process_remote_user_written_ty(return_ty);
    }

    fn collect_fn(
        &mut self,
        func: FunctionId,
        self_param: Option<BindingId>,
        params: &[Param<PatId>],
    ) {
        let sig = self.interner().liberate_late_bound_regions(
            func.into(),
            self.db.callable_item_signature(func.into()).instantiate_identity().skip_norm_wip(),
        );

        // C-variadic fns also have a `VaList` input that's not listed in `fn_sig`
        // (as it's created inside the body itself, not passed in from outside).
        let maybe_va_list =
            if sig.fn_sig_kind.c_variadic() { self.resolve_va_list() } else { None };
        let maybe_va_list = maybe_va_list.map(|va_list| {
            let region = self.table.next_region_var(
                params.last().expect("variadic function must have parameters").user_written.into(),
            );
            Ty::new_adt(self.interner(), va_list, GenericArgs::new_from_slice(&[region.into()]))
        });

        let mut param_tys = sig.inputs().iter().copied();

        if let Some(self_param) = self_param
            && let Some(ty) = param_tys.next()
        {
            let ty = self.process_remote_user_written_ty(ty);
            self.write_binding_ty(self_param, ty);
        }

        let mut param_tys = param_tys.chain(maybe_va_list);
        for pat in params {
            let ty = param_tys.next().unwrap_or_else(|| self.table.next_ty_var(Span::Dummy));
            let ty = self.process_remote_user_written_ty(ty);

            self.infer_top_pat(pat.formal, ty, PatOrigin::Param);
        }

        self.return_ty = self.process_remote_user_written_ty(sig.output());
        self.return_coercion = Some(CoerceMany::new(self.return_ty));
    }

    #[inline]
    pub(crate) fn interner(&self) -> DbInterner<'db> {
        self.table.interner()
    }

    #[inline]
    pub(crate) fn infcx(&self) -> &InferCtxt<'db> {
        &self.table.infer_ctxt
    }

    /// If `ty` is an error, returns an infer var instead. Otherwise, returns it.
    ///
    /// "Refreshing" types like this is useful for getting better types, but it is also
    /// very dangerous: we might create duplicate diagnostics, for example if we try
    /// to resolve it and fail. rustc doesn't do that for this reason (and is in general
    /// more strict with how it uses error types; an error type in inputs will almost
    /// always cause it to infer an error type in output, while we infer some type as much
    /// as we can).
    ///
    /// Unfortunately, we cannot allow ourselves to do that. Not only we more often work
    /// with incomplete code, we also have assists, for example "Generate constant", that
    /// will assume the inferred type is the expected type even if the expression itself
    /// cannot be inferred. Therefore, we choose a middle ground: refresh the type,
    /// but if we return a new var, mark it so that no diagnostics will be issued on it.
    fn insert_type_vars_shallow(&mut self, ty: Ty<'db>) -> Ty<'db> {
        if ty.is_ty_error() {
            let var = self.table.next_ty_var(Span::Dummy);

            // Suppress future errors on this var. Add more things here when we add more diagnostics.
            self.vars_emitted_type_must_be_known_for.insert(var.into());

            var
        } else {
            ty
        }
    }

    fn infer_body(&mut self, body_expr: ExprId) {
        match self.return_coercion {
            Some(_) => self.infer_return(body_expr),
            None => {
                _ = self.infer_expr_coerce(
                    body_expr,
                    &Expectation::has_type(self.return_ty),
                    ExprIsRead::Yes,
                )
            }
        }
    }

    fn write_expr_ty(&mut self, expr: ExprId, ty: Ty<'db>) {
        self.result.type_of_expr.insert(expr, ty.store());
    }

    pub(crate) fn write_expr_adj(&mut self, expr: ExprId, adjustments: Box<[Adjustment]>) {
        if adjustments.is_empty() {
            return;
        }
        let expr_adjustments = &mut self.result.expr_adjustments;
        if let Some(existing) = expr_adjustments.get_mut(&expr)
            && let [Adjustment { kind: Adjust::NeverToAny, target }] = &mut existing[..]
            && let [.., Adjustment { target: new_target, .. }] = &adjustments[..]
        {
            // NeverToAny coercion can target any type, so instead of adding a new
            // adjustment on top we can change the target.
            *target = new_target.clone();
            return;
        }
        expr_adjustments.insert(expr, adjustments);
    }

    pub(crate) fn write_method_resolution(
        &mut self,
        expr: ExprId,
        func: FunctionId,
        subst: GenericArgs<'db>,
    ) {
        self.result.method_resolutions.insert(expr, (func, subst.store()));
    }

    fn write_variant_resolution(&mut self, id: ExprOrPatIdPacked, variant: VariantId) {
        self.result.variant_resolutions.insert(id, variant);
    }

    fn write_value_resolution(&mut self, id: ExprOrPatIdPacked, value: ValueNs) {
        self.result.value_resolutions.insert(id, value);
    }

    fn write_assoc_resolution(
        &mut self,
        id: ExprOrPatIdPacked,
        item: CandidateId,
        subs: GenericArgs<'db>,
    ) {
        self.result.assoc_resolutions.insert(id, (item, subs.store()));
    }

    fn write_pat_ty(&mut self, pat: PatId, ty: Ty<'db>) {
        self.result.type_of_pat.insert(pat, ty.store());
    }

    fn write_binding_ty(&mut self, id: BindingId, ty: Ty<'db>) {
        self.result.type_of_binding.insert(id, ty.store());
    }

    pub(crate) fn push_diagnostic(&self, diagnostic: InferenceDiagnostic) {
        self.diagnostics.push(diagnostic);
    }

    /// Whether an error has been reported in this body so far.
    fn tainted_by_errors(&self) -> bool {
        self.diagnostics.any(InferenceDiagnostic::taints_body) || self.table.has_trait_errors()
    }

    fn record_deferred_call_resolution(
        &mut self,
        closure_def_id: ExprId,
        r: DeferredCallResolution<'db>,
    ) {
        self.deferred_call_resolutions.push((closure_def_id, r));
    }

    /// The calls deferred on `closure_def_id`, in the order they were recorded.
    fn remove_deferred_call_resolutions(
        &mut self,
        closure_def_id: ExprId,
    ) -> Vec<DeferredCallResolution<'db>> {
        self.deferred_call_resolutions
            .extract_if(.., |(closure, _)| *closure == closure_def_id)
            .map(|(_, resolution)| resolution)
            .collect()
    }

    fn with_ty_lowering<R>(&mut self, f: impl FnOnce(&mut TyLoweringContext<'db, '_>) -> R) -> R {
        let mut infer_vars = InferenceTyLoweringVarsCtx {
            table: &mut self.table,
            type_of_type_placeholder: &mut self.result.type_of_type_placeholder,
        };
        let mut ctx = TyLoweringContext::new(
            self.db,
            &self.resolver,
            self.store,
            &self.diagnostics,
            self.store_owner,
            self.generic_def,
            &self.generics,
            LifetimeElisionKind::Infer,
            self.allow_using_generic_params,
            &mut infer_vars,
            &self.defined_anon_consts,
            LifetimeLoweringMode::LateParam,
        );
        f(&mut ctx)
    }

    pub(crate) fn make_ty(&mut self, type_ref: TypeRefId) -> Ty<'db> {
        let ty = self.with_ty_lowering(|ctx| ctx.lower_ty(type_ref));
        self.process_user_written_ty(ty)
    }

    fn generics(&self) -> &Generics<'db> {
        self.generics.get_or_init(|| crate::generics::generics(self.db, self.generic_def))
    }

    fn identity_args(&self) -> GenericArgs<'db> {
        *self.identity_args.get_or_init(|| {
            GenericArgs::identity_for_item(self.interner(), self.generic_def.into())
        })
    }

    pub(crate) fn create_anon_const(&mut self, expr: ExprId, expected_ty: Ty<'db>) -> Const<'db> {
        never!(expected_ty.has_infer(), "cannot have infer vars in an anon const's ty");
        let konst = create_anon_const(
            self.interner(),
            self.store_owner,
            self.store,
            expr,
            &self.resolver,
            expected_ty,
            &|| self.generics(),
            Some(&mut |span| self.table.next_const_var(span)),
            self.lowering_mode,
            (!self.allow_using_generic_params).then_some(0),
            // The situation around body anon consts using generic params is complicated.
            // rustc disallows them all, except for array repeat expressions. However in another place even repeat exprs
            // are disallowed, so in practice they cause an error.
            false,
        );

        if let Ok(konst) = konst
            && let ConstKind::Alias(_, konst) = konst.kind()
            && let AliasConstKind::Anon { def_id: AnonConstIdWrapper(konst) } = konst.kind
        {
            self.defined_anon_consts.borrow_mut().push(konst);
        } else {
            self.write_expr_ty(expr, expected_ty);
        }

        // FIXME: Report an error if needed.
        konst.unwrap_or_else(|_| self.table.next_const_var(Span::Dummy))
    }

    pub(crate) fn make_path_as_const(&mut self, path: &Path) -> Const<'db> {
        let forbid_params_after = if self.allow_using_generic_params { None } else { Some(0) };
        // FIXME: Report errors.
        path_to_const(self.db, &self.resolver, &|| self.generics(), forbid_params_after, path)
            .unwrap_or_else(|_| self.table.next_const_var(Span::Dummy))
    }

    fn err_ty(&self) -> Ty<'db> {
        self.types.types.error
    }

    pub(crate) fn make_lifetime(&mut self, lifetime_ref: LifetimeRefId) -> Region<'db> {
        let lt = self.with_ty_lowering(|ctx| ctx.lower_lifetime(lifetime_ref));
        self.insert_type_vars(lt)
    }

    fn insert_type_vars<T>(&mut self, ty: T) -> T
    where
        T: TypeFoldable<DbInterner<'db>>,
    {
        self.table.insert_type_vars(ty)
    }

    /// Attempts to returns the deeply last field of nested structures, but
    /// does not apply any normalization in its search. Returns the same type
    /// if input `ty` is not a structure at all.
    fn struct_tail_without_normalization(&mut self, ty: Ty<'db>) -> Ty<'db> {
        self.struct_tail_with_normalize(ty, identity)
    }

    /// Returns the deeply last field of nested structures, or the same type if
    /// not a structure at all. Corresponds to the only possible unsized field,
    /// and its type can be used to determine unsizing strategy.
    ///
    /// This is parameterized over the normalization strategy (i.e. how to
    /// handle `<T as Trait>::Assoc` and `impl Trait`); pass the identity
    /// function to indicate no normalization should take place.
    fn struct_tail_with_normalize(
        &mut self,
        mut ty: Ty<'db>,
        mut normalize: impl FnMut(Ty<'db>) -> Ty<'db>,
    ) -> Ty<'db> {
        // FIXME: fetch the limit properly
        let recursion_limit = 10;
        for iteration in 0.. {
            if iteration > recursion_limit {
                return self.err_ty();
            }
            match ty.kind() {
                TyKind::Adt(adt_def, substs) => match adt_def.def_id() {
                    AdtId::StructId(struct_id) => {
                        match self
                            .db
                            .field_types(struct_id.into())
                            .values()
                            .next_back()
                            .map(|it| it.ty())
                        {
                            Some(field) => {
                                ty = field.instantiate(self.interner(), substs).skip_norm_wip();
                            }
                            None => break,
                        }
                    }
                    _ => break,
                },
                TyKind::Tuple(substs) => match substs.as_slice().split_last() {
                    Some((last_ty, _)) => ty = *last_ty,
                    None => break,
                },
                TyKind::Alias(..) => {
                    let normalized = normalize(ty);
                    if ty == normalized {
                        return ty;
                    } else {
                        ty = normalized;
                    }
                }
                _ => break,
            }
        }
        ty
    }

    /// Whenever you lower a user-written type, you should call this.
    fn process_user_written_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
        self.table.process_user_written_ty(ty)
    }

    /// The difference of this method from `process_user_written_ty()` is that this method doesn't register a well-formed obligation,
    /// while `process_user_written_ty()` should (but doesn't currently).
    fn process_remote_user_written_ty(&mut self, ty: Ty<'db>) -> Ty<'db> {
        self.table.process_remote_user_written_ty(ty)
    }

    fn shallow_resolve(&self, ty: Ty<'db>) -> Ty<'db> {
        self.table.shallow_resolve(ty)
    }

    pub(crate) fn resolve_vars_if_possible<T: TypeFoldable<DbInterner<'db>>>(&self, t: T) -> T {
        self.table.resolve_vars_if_possible(t)
    }

    pub(crate) fn structurally_resolve_type(
        &mut self,
        node: ExprOrPatIdPacked,
        ty: Ty<'db>,
    ) -> Ty<'db> {
        let result = self.table.try_structurally_resolve_type(node.into(), ty);
        if result.is_ty_var() { self.type_must_be_known_at_this_point(node, ty) } else { result }
    }

    pub(crate) fn innermost_tail_expr(&self, mut expr: ExprId) -> ExprId {
        while let Expr::Block { tail: Some(tail), label: None, .. } = self.store.expr(expr) {
            expr = tail;
        }
        expr
    }

    pub(crate) fn emit_type_mismatch(
        &mut self,
        node: ExprOrPatIdPacked,
        expected: Ty<'db>,
        found: Ty<'db>,
    ) {
        if self.result.nodes_with_type_mismatches.insert(node) {
            let at_error = self.types_at_error(expected, found);
            self.diagnostics.push(InferenceDiagnostic::TypeMismatch {
                node,
                expected: expected.store(),
                found: found.store(),
                at_error,
            });
        }
    }

    fn demand_eqtype(
        &mut self,
        id: ExprOrPatIdPacked,
        expected: Ty<'db>,
        actual: Ty<'db>,
    ) -> Result<(), ()> {
        let result = self
            .table
            .at(&ObligationCause::new(id))
            .eq(expected, actual)
            .map(|infer_ok| self.table.register_infer_ok(infer_ok));
        if result.is_err() {
            self.emit_type_mismatch(id, expected, actual);
        }
        result.map_err(drop)
    }

    fn demand_eqtype_fixme_no_diag(
        &mut self,
        expected: Ty<'db>,
        actual: Ty<'db>,
    ) -> Result<(), ()> {
        let result = self
            .table
            .at(&ObligationCause::dummy())
            .eq(expected, actual)
            .map(|infer_ok| self.table.register_infer_ok(infer_ok));
        result.map_err(drop)
    }

    fn demand_suptype(
        &mut self,
        id: ExprOrPatIdPacked,
        expected: Ty<'db>,
        actual: Ty<'db>,
    ) -> Result<(), ()> {
        let result = self
            .table
            .at(&ObligationCause::new(id))
            .sup(expected, actual)
            .map(|infer_ok| self.table.register_infer_ok(infer_ok));
        if result.is_err() {
            self.emit_type_mismatch(id, expected, actual);
        }
        result.map_err(drop)
    }

    fn demand_coerce(
        &mut self,
        expr: ExprId,
        checked_ty: Ty<'db>,
        expected: Ty<'db>,
        allow_two_phase: AllowTwoPhase,
        expr_is_read: ExprIsRead,
    ) -> Ty<'db> {
        let result = self.coerce(expr, checked_ty, expected, allow_two_phase, expr_is_read);
        if let Err(_err) = result {
            // FIXME: Emit diagnostic.
        }
        result.unwrap_or(self.types.types.error)
    }

    pub(crate) fn type_must_be_known_at_this_point(
        &mut self,
        node: ExprOrPatIdPacked,
        ty: Ty<'db>,
    ) -> Ty<'db> {
        if self.vars_emitted_type_must_be_known_for.insert(ty.into()) && !self.tainted_by_errors() {
            self.push_diagnostic(InferenceDiagnostic::TypeMustBeKnown {
                at_point: node.into(),
                top_term: None,
                kind: TypeAnnotationNeeded::E0282,
                ambiguity: None,
            });
        }
        self.types.types.error
    }

    pub(crate) fn require_type_is_sized(&mut self, ty: Ty<'db>, span: Span) {
        if !ty.references_non_lt_error()
            && let Some(sized_trait) = self.lang_items.Sized
        {
            self.table.register_bound(ty, sized_trait, ObligationCause::new(span));
        }
    }

    fn expr_ty(&self, expr: ExprId) -> Ty<'db> {
        self.result.expr_ty(expr)
    }

    fn expr_ty_after_adjustments(&self, e: ExprId) -> Ty<'db> {
        let mut ty = None;
        if let Some(it) = self.result.expr_adjustments.get(&e)
            && let Some(it) = it.last()
        {
            ty = Some(it.target.as_ref());
        }
        ty.unwrap_or_else(|| self.expr_ty(e))
    }

    fn resolve_variant(
        &mut self,
        node: ExprOrPatIdPacked,
        path: &Path,
        value_ns: bool,
    ) -> (Ty<'db>, Option<VariantId>) {
        let interner = self.interner();
        let mut vars_ctx = InferenceTyLoweringVarsCtx {
            table: &mut self.table,
            type_of_type_placeholder: &mut self.result.type_of_type_placeholder,
        };
        let mut ctx = TyLoweringContext::new(
            self.db,
            &self.resolver,
            self.store,
            &self.diagnostics,
            self.store_owner,
            self.generic_def,
            &self.generics,
            LifetimeElisionKind::Infer,
            self.allow_using_generic_params,
            &mut vars_ctx,
            &self.defined_anon_consts,
            LifetimeLoweringMode::LateParam,
        );

        if let Some(type_anchor) = path.type_anchor() {
            let mut segments = path.segments();
            if segments.is_empty() {
                return (self.types.types.error, None);
            }
            let (mut ty, type_ns) = ctx.lower_ty_ext(type_anchor);
            ty = ctx.expect_table().process_user_written_ty(ty);

            if let Some(TypeNs::SelfType(impl_)) = type_ns
                && let Some(trait_ref) = self.db.impl_trait(impl_)
                && let trait_ref = trait_ref.instantiate_identity().skip_norm_wip()
                && let Some(assoc_type) = trait_ref
                    .def_id
                    .0
                    .trait_items(self.db)
                    .associated_type_by_name(segments.first().unwrap().name)
            {
                // `<Self>::AssocType`
                let args = ctx.expect_table().infer_ctxt.fill_rest_fresh_args(
                    node.into(),
                    assoc_type.into(),
                    trait_ref.args,
                );
                let alias = Ty::new_alias(
                    interner,
                    IsRigid::No,
                    AliasTy::new_from_args(
                        interner,
                        AliasTyKind::Projection { def_id: assoc_type.into() },
                        args,
                    ),
                );
                ty = ctx.expect_table().try_structurally_resolve_type(node.into(), alias);
                segments = segments.skip(1);
            }

            let variant = match ty.as_adt() {
                Some((AdtId::StructId(id), _)) => id.into(),
                Some((AdtId::UnionId(id), _)) => id.into(),
                Some((AdtId::EnumId(id), _)) => {
                    if let Some(segment) = segments.first()
                        && let enum_data = id.enum_variants(self.db)
                        && let Some(variant) = enum_data.variant(segment.name)
                    {
                        // FIXME: Report error if there are generics on the variant.
                        segments = segments.skip(1);
                        variant.into()
                    } else {
                        return (self.types.types.error, None);
                    }
                }
                None => return (self.types.types.error, None),
            };

            if !segments.is_empty() {
                // FIXME: Report an error.
                return (self.types.types.error, None);
            } else {
                return (ty, Some(variant));
            }
        }

        let mut path_ctx = ctx.at_path(path, node);
        let interner = path_ctx.interner();
        let (resolution, unresolved) = if value_ns {
            let Some(res) = path_ctx.resolve_path_in_value_ns(HygieneId::ROOT) else {
                return (self.types.types.error, None);
            };
            match res {
                ResolveValueResult::ValueNs(value) => match value {
                    ValueNs::EnumVariantId(var) => {
                        let args = path_ctx.substs_from_path(var.into(), true, false, node.into());
                        drop(ctx);
                        let ty = self
                            .db
                            .ty(var.lookup(self.db).parent.into())
                            .instantiate(interner, args)
                            .skip_norm_wip();
                        let ty = self.insert_type_vars(ty);
                        return (ty, Some(var.into()));
                    }
                    ValueNs::StructId(strukt) => {
                        let args =
                            path_ctx.substs_from_path(strukt.into(), true, false, node.into());
                        drop(ctx);
                        let ty =
                            self.db.ty(strukt.into()).instantiate(interner, args).skip_norm_wip();
                        let ty = self.insert_type_vars(ty);
                        return (ty, Some(strukt.into()));
                    }
                    ValueNs::ImplSelf(impl_id) => (TypeNs::SelfType(impl_id), None),
                    _ => {
                        drop(ctx);
                        return (self.types.types.error, None);
                    }
                },
                ResolveValueResult::Partial(typens, unresolved) => (typens, Some(unresolved)),
            }
        } else {
            match path_ctx.resolve_path_in_type_ns() {
                Some((it, idx)) => (it, idx),
                None => return (self.types.types.error, None),
            }
        };
        return match resolution {
            TypeNs::AdtId(AdtId::StructId(strukt)) => {
                let args = path_ctx.substs_from_path(strukt.into(), true, false, node.into());
                drop(ctx);
                let ty = self.db.ty(strukt.into()).instantiate(interner, args).skip_norm_wip();
                let ty = self.insert_type_vars(ty);
                forbid_unresolved_segments(self, (ty, Some(strukt.into())), unresolved)
            }
            TypeNs::AdtId(AdtId::UnionId(u)) => {
                let args = path_ctx.substs_from_path(u.into(), true, false, node.into());
                drop(ctx);
                let ty = self.db.ty(u.into()).instantiate(interner, args).skip_norm_wip();
                let ty = self.insert_type_vars(ty);
                forbid_unresolved_segments(self, (ty, Some(u.into())), unresolved)
            }
            TypeNs::EnumVariantId(var) => {
                let args = path_ctx.substs_from_path(var.into(), true, false, node.into());
                drop(ctx);
                let ty = self
                    .db
                    .ty(var.lookup(self.db).parent.into())
                    .instantiate(interner, args)
                    .skip_norm_wip();
                let ty = self.insert_type_vars(ty);
                forbid_unresolved_segments(self, (ty, Some(var.into())), unresolved)
            }
            TypeNs::SelfType(impl_id) => {
                let mut ty = self.db.impl_self_ty(impl_id).instantiate_identity().skip_norm_wip();

                let Some(remaining_idx) = unresolved else {
                    drop(ctx);
                    let Some(mod_path) = path.mod_path() else {
                        never!("resolver should always resolve lang item paths");
                        return (self.types.types.error, None);
                    };
                    return self.resolve_variant_on_alias(node, ty, None, mod_path);
                };

                let mut remaining_segments = path.segments().skip(remaining_idx);

                if remaining_segments.len() >= 2 {
                    path_ctx.ignore_last_segment();
                }

                // We need to try resolving unresolved segments one by one because each may resolve
                // to a projection, which `TyLoweringContext` cannot handle on its own.
                let mut tried_resolving_once = false;
                while let Some(current_segment) = remaining_segments.first() {
                    // If we can resolve to an enum variant, it takes priority over associated type
                    // of the same name.
                    let ty_resolved =
                        path_ctx.expect_table().try_structurally_resolve_type(node.into(), ty);
                    if let TyKind::Adt(adt_def, _) = ty_resolved.kind()
                        && let AdtId::EnumId(id) = adt_def.def_id()
                    {
                        let enum_data = id.enum_variants(self.db);
                        if let Some(variant) = enum_data.variant(current_segment.name) {
                            return if remaining_segments.len() == 1 {
                                (ty_resolved, Some(variant.into()))
                            } else {
                                // We still have unresolved paths, but enum variants never have
                                // associated types!
                                // FIXME: Report an error.
                                (self.types.types.error, None)
                            };
                        }
                    }

                    if tried_resolving_once {
                        // FIXME: with `inherent_associated_types` this is allowed, but our `lower_partly_resolved_path()`
                        // will need to be updated to err at the correct segment.
                        break;
                    }

                    // `lower_partly_resolved_path()` returns `None` as type namespace unless
                    // `remaining_segments` is empty, which is never the case here. We don't know
                    // which namespace the new `ty` is in until normalized anyway.
                    (ty, _) = path_ctx.lower_partly_resolved_path(resolution, true, node.into());
                    tried_resolving_once = true;

                    ty = path_ctx.expect_table().process_user_written_ty(ty);
                    if ty.is_ty_error() {
                        return (self.types.types.error, None);
                    }

                    remaining_segments = remaining_segments.skip(1);
                }
                drop(ctx);
                let ty = self.table.try_structurally_resolve_type(node.into(), ty);
                let variant = ty.as_adt().and_then(|(id, _)| match id {
                    AdtId::StructId(s) => Some(VariantId::StructId(s)),
                    AdtId::UnionId(u) => Some(VariantId::UnionId(u)),
                    AdtId::EnumId(_) => {
                        // FIXME Error E0071, expected struct, variant or union type, found enum `Foo`
                        None
                    }
                });
                (ty, variant)
            }
            TypeNs::TraitId(_) => {
                let Some(remaining_idx) = unresolved else {
                    return (self.types.types.error, None);
                };

                let remaining_segments = path.segments().skip(remaining_idx);

                if remaining_segments.len() >= 2 {
                    path_ctx.ignore_last_segment();
                }

                let (mut ty, _) =
                    path_ctx.lower_partly_resolved_path(resolution, true, node.into());
                ty = ctx.expect_table().process_user_written_ty(ty);
                ty = ctx.expect_table().try_structurally_resolve_type(node.into(), ty);
                if let Some(segment) = remaining_segments.get(1)
                    && let Some((AdtId::EnumId(id), _)) = ty.as_adt()
                {
                    let enum_data = id.enum_variants(self.db);
                    if let Some(variant) = enum_data.variant(segment.name) {
                        return if remaining_segments.len() == 2 {
                            (ty, Some(variant.into()))
                        } else {
                            // We still have unresolved paths, but enum variants never have
                            // associated types!
                            // FIXME: Report an error.
                            (self.types.types.error, None)
                        };
                    }
                }

                let variant = ty.as_adt().and_then(|(id, _)| match id {
                    AdtId::StructId(s) => Some(VariantId::StructId(s)),
                    AdtId::UnionId(u) => Some(VariantId::UnionId(u)),
                    AdtId::EnumId(_) => {
                        // FIXME Error E0071, expected struct, variant or union type, found enum `Foo`
                        None
                    }
                });
                (ty, variant)
            }
            TypeNs::TypeAliasId(it) => {
                let Some(mod_path) = path.mod_path() else {
                    never!("resolver should always resolve lang item paths");
                    return (self.types.types.error, None);
                };
                let args =
                    path_ctx.substs_from_path_segment(it.into(), true, None, false, node.into());
                let interner = path_ctx.interner();
                drop(ctx);
                let ty = self.db.ty(it.into()).instantiate(interner, args).skip_norm_wip();
                let ty = self.insert_type_vars(ty);

                self.resolve_variant_on_alias(node, ty, unresolved, mod_path)
            }
            TypeNs::AdtSelfType(_) => {
                // FIXME this could happen in array size expressions, once we're checking them
                (self.types.types.error, None)
            }
            TypeNs::GenericParam(_) => {
                // FIXME potentially resolve assoc type
                (self.types.types.error, None)
            }
            TypeNs::AdtId(AdtId::EnumId(_)) | TypeNs::BuiltinType(_) | TypeNs::ModuleId(_) => {
                // FIXME diagnostic
                (self.types.types.error, None)
            }
        };

        fn forbid_unresolved_segments<'db>(
            ctx: &InferenceContext<'db>,
            result: (Ty<'db>, Option<VariantId>),
            unresolved: Option<usize>,
        ) -> (Ty<'db>, Option<VariantId>) {
            if unresolved.is_none() {
                result
            } else {
                // FIXME diagnostic
                (ctx.types.types.error, None)
            }
        }
    }

    fn resolve_variant_on_alias(
        &mut self,
        node: ExprOrPatIdPacked,
        ty: Ty<'db>,
        unresolved: Option<usize>,
        path: &ModPath,
    ) -> (Ty<'db>, Option<VariantId>) {
        let remaining = unresolved.map(|it| path.segments()[it..].len()).filter(|it| it > &0);
        let ty = self.table.try_structurally_resolve_type(node.into(), ty);
        match remaining {
            None => {
                let variant = ty.as_adt().and_then(|(adt_id, _)| match adt_id {
                    AdtId::StructId(s) => Some(VariantId::StructId(s)),
                    AdtId::UnionId(u) => Some(VariantId::UnionId(u)),
                    AdtId::EnumId(_) => {
                        // FIXME Error E0071, expected struct, variant or union type, found enum `Foo`
                        None
                    }
                });
                (ty, variant)
            }
            Some(1) => {
                let segment = path.segments().last().unwrap();
                // this could be an enum variant or associated type
                if let Some((AdtId::EnumId(enum_id), _)) = ty.as_adt() {
                    let enum_data = enum_id.enum_variants(self.db);
                    if let Some(variant) = enum_data.variant(segment) {
                        return (ty, Some(variant.into()));
                    }
                }
                // FIXME potentially resolve assoc type
                (self.err_ty(), None)
            }
            Some(_) => {
                // FIXME diagnostic
                (self.err_ty(), None)
            }
        }
    }

    fn resolve_va_list(&self) -> Option<AdtId> {
        let struct_ = self.lang_items.VaList?;
        Some(struct_.into())
    }

    /// Filled once each time the walk enters a block module that adds traits; every method call
    /// inside it then takes an `Rc` clone, with no allocation or hashing.
    pub(crate) fn get_traits_in_scope(
        &self,
    ) -> Either<std::rc::Rc<FxHashSet<TraitId>>, &FxHashSet<TraitId>> {
        let module = self.resolver.module();
        let mut cache = self.block_traits.borrow_mut();
        let (key, has_block_traits, traits) = &mut *cache;
        if *key != Some(module) {
            *key = Some(module);
            let mut b_traits = self.resolver.traits_in_scope_from_block_scopes(self.db).peekable();
            *has_block_traits = b_traits.peek().is_some();
            if *has_block_traits {
                let set = std::rc::Rc::make_mut(traits);
                set.clear();
                set.extend(self.traits_in_scope.iter().copied());
                set.extend(b_traits);
            }
        }
        if *has_block_traits {
            Either::Left(traits.clone())
        } else {
            Either::Right(&self.traits_in_scope)
        }
    }

    fn has_applicable_non_exhaustive(&self, def: AttrDefId) -> bool {
        AttrFlags::query(self.db, def).contains(AttrFlags::NON_EXHAUSTIVE)
            && def.krate(self.db) != self.krate()
    }
}

/// When inferring an expression, we propagate downward whatever type hint we
/// are able in the form of an `Expectation`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) enum Expectation<'db> {
    None,
    HasType(Ty<'db>),
    Castable(Ty<'db>),
    RValueLikeUnsized(Ty<'db>),
}

impl<'db> Expectation<'db> {
    /// The expectation that the type of the expression needs to equal the given
    /// type.
    fn has_type(ty: Ty<'db>) -> Self {
        if ty.is_ty_error() {
            // FIXME: get rid of this?
            Expectation::None
        } else {
            Expectation::HasType(ty)
        }
    }

    /// The following explanation is copied straight from rustc:
    /// Provides an expectation for an rvalue expression given an *optional*
    /// hint, which is not required for type safety (the resulting type might
    /// be checked higher up, as is the case with `&expr` and `box expr`), but
    /// is useful in determining the concrete type.
    ///
    /// The primary use case is where the expected type is a fat pointer,
    /// like `&[isize]`. For example, consider the following statement:
    ///
    ///     let it: &[isize] = &[1, 2, 3];
    ///
    /// In this case, the expected type for the `&[1, 2, 3]` expression is
    /// `&[isize]`. If however we were to say that `[1, 2, 3]` has the
    /// expectation `ExpectHasType([isize])`, that would be too strong --
    /// `[1, 2, 3]` does not have the type `[isize]` but rather `[isize; 3]`.
    /// It is only the `&[1, 2, 3]` expression as a whole that can be coerced
    /// to the type `&[isize]`. Therefore, we propagate this more limited hint,
    /// which still is useful, because it informs integer literals and the like.
    /// See the test case `test/ui/coerce-expect-unsized.rs` and #20169
    /// for examples of where this comes up,.
    fn rvalue_hint(ctx: &mut InferenceContext<'db>, ty: Ty<'db>) -> Self {
        match ctx.struct_tail_without_normalization(ty).kind() {
            TyKind::Slice(_) | TyKind::Str | TyKind::Dynamic(..) => {
                Expectation::RValueLikeUnsized(ty)
            }
            _ => Expectation::has_type(ty),
        }
    }

    /// This expresses no expectation on the type.
    fn none() -> Self {
        Expectation::None
    }

    fn resolve(&self, table: &unify::InferenceTable<'db>) -> Expectation<'db> {
        match self {
            Expectation::None => Expectation::None,
            Expectation::HasType(t) => Expectation::HasType(table.shallow_resolve(*t)),
            Expectation::Castable(t) => Expectation::Castable(table.shallow_resolve(*t)),
            Expectation::RValueLikeUnsized(t) => {
                Expectation::RValueLikeUnsized(table.shallow_resolve(*t))
            }
        }
    }

    fn to_option(&self, table: &unify::InferenceTable<'db>) -> Option<Ty<'db>> {
        match self.resolve(table) {
            Expectation::None => None,
            Expectation::HasType(t)
            | Expectation::Castable(t)
            | Expectation::RValueLikeUnsized(t) => Some(t),
        }
    }

    fn only_has_type(&self, table: &mut unify::InferenceTable<'db>) -> Option<Ty<'db>> {
        match self {
            Expectation::HasType(t) => Some(table.resolve_vars_if_possible(*t)),
            Expectation::Castable(_) | Expectation::RValueLikeUnsized(_) | Expectation::None => {
                None
            }
        }
    }

    fn coercion_target_type(&self, table: &mut unify::InferenceTable<'db>, span: Span) -> Ty<'db> {
        self.only_has_type(table).unwrap_or_else(|| table.next_ty_var(span))
    }

    /// Comment copied from rustc:
    /// Disregard "castable to" expectations because they
    /// can lead us astray. Consider for example `if cond
    /// {22} else {c} as u8` -- if we propagate the
    /// "castable to u8" constraint to 22, it will pick the
    /// type 22u8, which is overly constrained (c might not
    /// be a u8). In effect, the problem is that the
    /// "castable to" expectation is not the tightest thing
    /// we can say, so we want to drop it in this case.
    /// The tightest thing we can say is "must unify with
    /// else branch". Note that in the case of a "has type"
    /// constraint, this limitation does not hold.
    ///
    /// If the expected type is just a type variable, then don't use
    /// an expected type. Otherwise, we might write parts of the type
    /// when checking the 'then' block which are incompatible with the
    /// 'else' branch.
    fn adjust_for_branches(
        &self,
        table: &mut unify::InferenceTable<'db>,
        span: Span,
    ) -> Expectation<'db> {
        match *self {
            Expectation::HasType(ety) => {
                let ety = table.try_structurally_resolve_type(span, ety);
                if ety.is_ty_var() { Expectation::None } else { Expectation::HasType(ety) }
            }
            Expectation::RValueLikeUnsized(ety) => Expectation::RValueLikeUnsized(ety),
            _ => Expectation::None,
        }
    }
}

/// rustc's `Diverges` (`rustc_hir_typeck/src/diverges.rs`): whether executing a node may exit
/// normally, versus return, break or panic, which leave dead code in their wake.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Diverges {
    /// Some cases converge, or a CFG would be needed to tell.
    Maybe,
    /// Known to diverge, so not to reach the next sibling or the parent.
    Always {
        /// What diverged (`return`, `break`, ...).
        span: ExprOrPatId,
        /// A note for the warning in place of "any code following this expression is
        /// unreachable".
        custom_note: Option<&'static str>,
    },
    /// `Always`, with the warning already given.
    WarnedAlways,
}

impl Diverges {
    fn always(span: impl Into<ExprOrPatId>) -> Diverges {
        Diverges::Always { span: span.into(), custom_note: None }
    }

    fn is_always(self) -> bool {
        matches!(self, Diverges::Always { .. } | Diverges::WarnedAlways)
    }
}

impl std::ops::BitAnd for Diverges {
    type Output = Self;
    fn bitand(self, other: Self) -> Self {
        match (self, other) {
            (x @ Self::Maybe, _) => x,
            (_, x @ Self::Maybe) => x,
            (x @ Self::Always { .. }, Self::Always { .. }) => x,
            (Self::WarnedAlways, x) => x,
            (x, Self::WarnedAlways) => x,
        }
    }
}

impl std::ops::BitOr for Diverges {
    type Output = Self;
    fn bitor(self, other: Self) -> Self {
        match (self, other) {
            (x @ Self::WarnedAlways, _) => x,
            (_, x @ Self::WarnedAlways) => x,
            (x @ Self::Always { .. }, Self::Always { .. }) => x,
            (Self::Maybe, x) => x,
            (x, Self::Maybe) => x,
        }
    }
}

/// rustc's `is_automatically_derived` of the impl a body is in: `warn_if_unreachable` is quiet
/// there (a derived `Clone` or `PartialEq` of a type holding `!`).
fn in_automatically_derived_impl(db: &Db, owner: InferBodyId<'_>) -> bool {
    let container = match owner {
        InferBodyId::DefWithBodyId(DefWithBodyId::FunctionId(it)) => it.lookup(db).container,
        InferBodyId::DefWithBodyId(DefWithBodyId::ConstId(it)) => it.lookup(db).container,
        _ => return false,
    };
    match container {
        hir_def::ItemContainerId::ImplId(it) => {
            AttrFlags::has_word(db, it.into(), "automatically_derived")
        }
        _ => false,
    }
}

impl std::ops::BitAndAssign for Diverges {
    fn bitand_assign(&mut self, other: Self) {
        *self = *self & other;
    }
}

impl std::ops::BitOrAssign for Diverges {
    fn bitor_assign(&mut self, other: Self) {
        *self = *self | other;
    }
}

#[cfg(test)]
mod tests {
    use rustc_hash::FxHashMap;

    use super::hash_unordered;

    fn hash(map: &FxHashMap<u32, u32>) -> (u64, u64) {
        let mut hasher = salsa::StableHasher::new();
        hash_unordered(map.iter(), &mut hasher);
        hasher.finish128()
    }

    #[test]
    fn equal_maps_hash_the_same_whatever_their_order() {
        let forward: FxHashMap<u32, u32> = (0..64).map(|it| (it, it * 7)).collect();
        let mut backward = FxHashMap::default();
        backward.reserve(1024);
        for it in (0..64).rev() {
            backward.insert(it, it * 7);
        }
        assert_eq!(forward, backward);
        assert_eq!(hash(&forward), hash(&backward));
    }

    #[test]
    fn a_changed_entry_changes_the_hash() {
        let map: FxHashMap<u32, u32> = (0..64).map(|it| (it, it * 7)).collect();
        let mut changed = map.clone();
        changed.insert(5, 0);
        assert_ne!(hash(&map), hash(&changed));
        let mut moved = map.clone();
        moved.remove(&5);
        moved.insert(64, 35);
        assert_ne!(hash(&map), hash(&moved));
    }
}
