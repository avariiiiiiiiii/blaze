//! Things related to the Interner in the next-trait-solver.

use std::{fmt, marker::PhantomData, num::NonZeroU32, ops::ControlFlow};

use either::Either;
use rustc_abi::ReprOptions;
use rustc_ast_ir::{FloatTy, IntTy, UintTy};
pub use tls_cache::clear_tls_solver_cache;
pub use tls_db::{attach_db, attach_db_allow_change, with_attached_db};

use base_db::Crate;
use hir_def::{
    AdtId, CallableDefId, EnumId, GenericDefId, GenericParamId, HasModule, ItemContainerId,
    StructId, TraitId, TypeAliasId, UnionId, VariantId,
    attrs::AttrFlags,
    expr_store::{ExpressionStore, StoreVisitor},
    hir::{ClosureKind as HirClosureKind, CoroutineKind as HirCoroutineKind, ExprId, PatId},
    lang_item::LangItems,
    signatures::{
        EnumFlags, EnumSignature, ImplFlags, ImplSignature, StructFlags, StructSignature,
        TraitFlags, TraitSignature, UnionSignature,
    },
};
use rustc_abi::ExternAbi;
use rustc_index::bit_set::DenseBitSet;
use intern::sym;
use rustc_abi::Size;
use rustc_type_ir::{
    AliasConstKind, BoundVar, BoundVarIndexKind, CanonicalParamEnvCache, ConstKind,
    CoroutineWitnessTypes, DebruijnIndex, EarlyBinder, Flags, FnSigKind, GenericArgKind,
    GenericTypeVisitable, ImplPolarity, InferTy, Interner, RegionKind, RegionVid, TraitRef,
    TypeFlags, TypeVisitableExt, ValTreeKind, Variance, VisitorResult,
    error::TypeError,
    fast_reject,
    inherent::{self, GenericsOf, IntoKind, SliceLike as _, Span as _, Ty as _},
    lang_items::{SolverAdtLangItem, SolverProjectionLangItem, SolverTraitLangItem},
    search_graph::RequiredDepth,
    solve::{AdtDestructorKind, QueryResult, SizedTraitKind},
    try_visit,
};

use crate::{
    InferBodyId, Span,
    db::{HirDatabase, InternedClosure, InternedCoroutineId},
    lower::GenericPredicates,
    method_resolution::{SelfTyKey, TraitImpls},
    next_solver::{
        AdtIdWrapper, AliasTermKind, AnonConstIdWrapper, AnyImplId, BoundConst,
        CallableIdWrapper, CanonicalVarKind, ClosureIdWrapper, Consts, CoroutineClosureIdWrapper,
        CoroutineIdWrapper, Ctor, FnSig, FreeConstAliasId, FreeTermAliasId, FreeTyAliasId,
        FxIndexMap, ImplOrTraitAssocConstId, ImplOrTraitAssocTermId, ImplOrTraitAssocTyId,
        InherentAssocConstId, InherentAssocTermId, InherentAssocTyId, InternedConst,
        InternedRegion, LateParamRegionKind, OpaqueTyIdWrapper, OpaqueTypeKey, RegionAssumptions,
        Scalar, ScalarInt, SimplifiedType, SolverContext, SolverDefIds, TermId, TraitAssocConstId,
        TraitAssocTermId, TraitAssocTyId, TraitIdWrapper, TypeAliasIdWrapper,
        util::{ItemBounds, impl_super_outlives},
    },
};

use super::{
    Binder, BoundExistentialPredicates, BoundTy, BoundTyKind, Clause, ClauseKind, Clauses, Const,
    ErrorGuaranteed, ExprConst, ExternalConstraints, GenericArg, GenericArgs, ParamConst, ParamEnv,
    ParamTy, PredefinedOpaques, Predicate, SolverDefId, Term, Ty, TyKind, Tys, ValTree, ValueConst,
    abi::Safety,
    fold::{BoundVarReplacer, BoundVarReplacerDelegate, FnMutDelegate},
    generics::{Generics, generics},
    persist,
    region::{BoundRegion, BoundRegionKind, EarlyParamRegion, Region},
    util::sizedness_constraint_for_ty,
};

/// A list of interned values, as an index into a [`table::Lists`] of its own: `$storage` holds the
/// table, `$name<'db>` is the list, `$stored_name` the list as kept in a query result.
macro_rules! interned_slice {
    ($storage:ident, $name:ident, $stored_name:ident, $default_types_field:ident, $ty_db:ty, $ty_static:ty $(,)?) => {
        const _: () = {
            #[allow(unused_lifetimes)]
            fn _ensure_correct_types<'db: 'static>(v: $ty_db) -> $ty_static { v }
        };

        // SAFETY: `$ty_static` is `$ty_db` with `'db` as `'static` (`_ensure_correct_types`
        // above), which bounds no reference in it (`table::Erase`).
        #[allow(unused_lifetimes)]
        unsafe impl<'db> $crate::next_solver::interner::table::Erase for $ty_db {
            type Static = $ty_static;
        }

        /// Where every interned [`$name`] is.
        pub struct $storage;

        impl $storage {
            #[inline]
            fn table() -> &'static $crate::next_solver::interner::table::Lists<$ty_static> {
                static TABLE: $crate::next_solver::interner::table::Lists<$ty_static> =
                    $crate::next_solver::interner::table::Lists::new(::std::stringify!($name));
                &TABLE
            }
        }

        #[derive(Clone, Copy, PartialEq, Eq, Hash)]
        #[repr(transparent)]
        pub struct $name<'db> {
            index: ::std::num::NonZeroU32,
            _db: ::std::marker::PhantomData<&'db ()>,
        }

        const _: () = ::std::assert!(::std::mem::size_of::<Option<$name<'static>>>() == 4);

        impl<'db> std::fmt::Debug for $name<'db> {
            fn fmt(&self, fmt: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.as_slice().fmt(fmt)
            }
        }

        impl<'db> $name<'db> {
            #[inline]
            pub fn empty() -> Self {
                $crate::next_solver::default_types().empty.$default_types_field
            }

            #[inline]
            pub fn new_from_slice(slice: &[$ty_db]) -> Self {
                if slice.is_empty() {
                    // Common case: avoid looking up the empty slice.
                    Self::empty()
                } else {
                    Self::new_from_slice_no_empty_check(slice)
                }
            }

            /// Same as [`Self::new_from_slice()`] but won't use the global empty slice `slice.is_empty()`, because someone
            /// needs to intern the global slice as well.
            #[inline]
            pub(crate) fn new_from_slice_no_empty_check(slice: &[$ty_db]) -> Self {
                Self::at($storage::table().intern(slice))
            }

            #[inline]
            pub fn new_from_iter<I, T>(args: I) -> T::Output
            where
                I: IntoIterator<Item = T>,
                T: ::rustc_type_ir::CollectAndApply<$ty_db, Self>,
            {
                ::rustc_type_ir::CollectAndApply::collect_and_apply(args.into_iter(), |g| {
                    Self::new_from_slice(g)
                })
            }

            #[inline]
            pub fn as_slice(self) -> &'db [$ty_db] {
                $storage::table().get(self.index)
            }

            #[inline]
            pub fn iter(self) -> ::std::iter::Copied<::std::slice::Iter<'db, $ty_db>> {
                self.as_slice().iter().copied()
            }

            #[inline]
            pub fn len(self) -> usize {
                self.as_slice().len()
            }

            #[inline]
            pub fn is_empty(self) -> bool {
                self.as_slice().is_empty()
            }

            /// The list's place in its table: dense from 1, the same for equal lists, for the
            /// life of the process.
            #[inline]
            pub fn index(self) -> u32 {
                self.index.get()
            }

            /// The list at `index`, which [`Self::index`] gave. Panics on any other number.
            pub fn from_index(_interner: $crate::next_solver::DbInterner<'db>, index: u32) -> Self {
                match ::std::num::NonZeroU32::new(index) {
                    Some(index) if $storage::table().published(index) => Self::at(index),
                    _ => ::std::panic!(
                        "{index} is not the index of an interned {}",
                        ::std::stringify!($name)
                    ),
                }
            }

            /// The list of a row of the table.
            #[inline]
            pub(crate) fn at(index: ::std::num::NonZeroU32) -> Self {
                Self { index, _db: ::std::marker::PhantomData }
            }

            pub(crate) fn row(self) -> ::std::num::NonZeroU32 {
                self.index
            }
        }

        impl<'db> IntoIterator for $name<'db> {
            type IntoIter = ::std::iter::Copied<::std::slice::Iter<'db, $ty_db>>;
            type Item = $ty_db;
            #[inline]
            fn into_iter(self) -> Self::IntoIter { self.iter() }
        }

        impl<'db> ::std::ops::Deref for $name<'db> {
            type Target = [$ty_db];

            #[inline]
            fn deref(&self) -> &Self::Target {
                (*self).as_slice()
            }
        }

        impl<'db> rustc_type_ir::inherent::SliceLike for $name<'db> {
            type Item = $ty_db;

            type IntoIter = ::std::iter::Copied<::std::slice::Iter<'db, $ty_db>>;

            #[inline]
            fn iter(self) -> Self::IntoIter {
                self.iter()
            }

            #[inline]
            fn as_slice(&self) -> &[Self::Item] {
                (*self).as_slice()
            }
        }

        impl<'db> Default for $name<'db> {
            #[inline]
            fn default() -> Self {
                $name::empty()
            }
        }

        impl<'db, V: $crate::next_solver::interner::WorldExposer>
            rustc_type_ir::GenericTypeVisitable<V> for $name<'db>
        {
            #[inline]
            fn generic_visit_with(&self, visitor: &mut V) {
                if visitor.on_interned(::std::stringify!($name), self.index.get()).is_continue() {
                    self.as_slice().iter().for_each(|it| it.generic_visit_with(visitor));
                }
            }
        }

        $crate::next_solver::interner::impl_stored_interned_slice!($name, $stored_name);
    };
}
pub(crate) use interned_slice;

/// `$stored_name`, the list `$name` as kept in a query result: its index, which stays valid in
/// every revision, as an interned value is never freed.
macro_rules! impl_stored_interned_slice {
    ( $name:ident, $stored_name:ident $(,)? ) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash)]
        pub struct $stored_name {
            index: ::std::num::NonZeroU32,
        }

        impl $stored_name {
            #[inline]
            pub fn as_ref<'db>(&self) -> $name<'db> {
                $name::at(self.index)
            }
        }

        // SAFETY: `$stored_name` is a row number and borrows nothing, and the row it names is
        // never freed, so it is valid in every revision (unlike `$name`, which carries `'db`).
        unsafe impl salsa::SalsaValue for $stored_name {}

        impl std::fmt::Debug for $stored_name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.as_ref().fmt(f)
            }
        }

        impl $name<'_> {
            #[inline]
            pub fn store(self) -> $stored_name {
                $stored_name { index: self.index }
            }
        }
    };
}
pub(crate) use impl_stored_interned_slice;

macro_rules! impl_foldable_for_interned_slice {
    ($name:ident) => {
        impl<'db> ::rustc_type_ir::TypeVisitable<DbInterner<'db>> for $name<'db> {
            fn visit_with<V: rustc_type_ir::TypeVisitor<DbInterner<'db>>>(
                &self,
                visitor: &mut V,
            ) -> V::Result {
                use rustc_ast_ir::visit::VisitorResult;
                rustc_ast_ir::walk_visitable_list!(visitor, (*self).iter());
                V::Result::output()
            }
        }

        impl<'db> rustc_type_ir::TypeFoldable<DbInterner<'db>> for $name<'db> {
            fn try_fold_with<F: rustc_type_ir::FallibleTypeFolder<DbInterner<'db>>>(
                self,
                folder: &mut F,
            ) -> Result<Self, F::Error> {
                Self::new_from_iter(self.iter().map(|it| it.try_fold_with(folder)))
            }
            fn fold_with<F: rustc_type_ir::TypeFolder<DbInterner<'db>>>(
                self,
                folder: &mut F,
            ) -> Self {
                Self::new_from_iter(self.iter().map(|it| it.fold_with(folder)))
            }
        }
    };
}
pub(crate) use impl_foldable_for_interned_slice;

macro_rules! impl_foldable_for_stored_type {
    ($name:ident) => {
        impl<'db> ::rustc_type_ir::TypeVisitable<DbInterner<'db>> for $name {
            fn visit_with<V: rustc_type_ir::TypeVisitor<DbInterner<'db>>>(
                &self,
                visitor: &mut V,
            ) -> V::Result {
                self.as_ref().visit_with(visitor)
            }
        }

        impl<'db> rustc_type_ir::TypeFoldable<DbInterner<'db>> for $name {
            fn try_fold_with<F: rustc_type_ir::FallibleTypeFolder<DbInterner<'db>>>(
                self,
                folder: &mut F,
            ) -> Result<Self, F::Error> {
                Ok(self.as_ref().try_fold_with(folder)?.store())
            }
            fn fold_with<F: rustc_type_ir::TypeFolder<DbInterner<'db>>>(
                self,
                folder: &mut F,
            ) -> Self {
                self.as_ref().fold_with(folder).store()
            }
        }
    };
}
pub(crate) use impl_foldable_for_stored_type;

/// `$stored_name`, the interned `$name` as kept in a query result: its index, which stays valid
/// in every revision, as an interned value is never freed. `$name` has an `index: NonZeroU32`
/// field and an `at(NonZeroU32)` constructor.
macro_rules! impl_stored_interned {
    ( $name:ident, $stored_name:ident $(,)? ) => {
        #[derive(Clone, Copy, PartialEq, Eq, Hash, ::salsa::SalsaValue)]
        pub struct $stored_name {
            index: ::std::num::NonZeroU32,
        }

        const _: () = ::std::assert!(::std::mem::size_of::<Option<$stored_name>>() == 4);

        impl $stored_name {
            #[inline]
            pub fn as_ref<'db>(&self) -> $name<'db> {
                $name::at(self.index)
            }
        }

        impl std::fmt::Debug for $stored_name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.as_ref().fmt(f)
            }
        }

        impl $name<'_> {
            #[inline]
            pub fn store(self) -> $stored_name {
                $stored_name { index: self.index }
            }
        }
    };
}
pub(crate) use impl_stored_interned;

/// A visitor of the interned values a value holds. Visitables call [`Self::on_interned`] with
/// the table and index of each interned value they reach, before its parts, and skip the parts
/// on [`ControlFlow::Break`] (a value already seen): the walk a reachability pass over the
/// tables makes.
pub trait WorldExposer {
    fn on_interned(&mut self, table: &'static str, index: u32) -> ControlFlow<()>;
}

#[derive(Debug, Copy, Clone)]
pub struct DbInterner<'db> {
    pub db: &'db dyn HirDatabase,
    krate: Option<Crate>,
    lang_items: Option<&'db LangItems>,
    /// The crate's `#![recursion_limit]`, or rustc's default. The solver asks for it with every
    /// goal, so it is read once.
    recursion_limit: u32,
    /// Which solver cache is this interner's, see [`tls_cache`].
    solved: tls_cache::Solved,
    /// The solver answers with the region constraints proving a goal takes, as borrowck needs;
    /// otherwise regions are not looked at.
    regions: bool,
}

// The database is `Send + Sync` (`salsa::Database`), so an interner is, by the auto traits.
const _: fn() = || {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<DbInterner<'static>>();
};

impl<'db> DbInterner<'db> {
    /// Creates a new interner without an active crate. Good only for interning things, not for trait solving etc..
    /// As a rule of thumb, when you create an `InferCtxt`, you need to provide the crate (and the block).
    ///
    /// Elaboration is a special kind: it needs lang items (for `Sized`), therefore it needs `new_with()`.
    pub fn new_no_crate(db: &'db dyn HirDatabase) -> Self {
        DbInterner {
            db,
            krate: None,
            lang_items: None,
            recursion_limit: 128,
            solved: tls_cache::Solved::new(),
            regions: false,
        }
    }

    pub fn new_with(db: &'db dyn HirDatabase, krate: Crate) -> DbInterner<'db> {
        DbInterner {
            db,
            krate: Some(krate),
            // As an approximation, when we call `new_with` we're trait solving, therefore we need the lang items.
            // This is also convenient since here we have a starting crate but not in `new_no_crate`.
            lang_items: Some(hir_def::lang_item::lang_items(db, krate)),
            recursion_limit: hir_def::nameres::crate_def_map(db, krate).recursion_limit(),
            solved: tls_cache::Solved::new(),
            regions: false,
        }
    }

    /// This interner, with the solver answering with the region constraints of what it proves.
    /// It solves into a cache of its own, as an answer without them would be wrong for it.
    pub fn with_regions(self) -> DbInterner<'db> {
        DbInterner { regions: true, solved: tls_cache::Solved::new(), ..self }
    }

    #[inline]
    pub(crate) fn regions(&self) -> bool {
        self.regions
    }

    #[inline]
    pub fn db(&self) -> &'db dyn HirDatabase {
        self.db
    }

    #[inline]
    #[track_caller]
    pub fn lang_items(&self) -> &'db LangItems {
        self.lang_items.expect(
            "Must have `DbInterner::lang_items`.\n\n\
            Note: you might have called `DbInterner::new_no_crate()` \
            where you should've called `DbInterner::new_with()`",
        )
    }

    #[inline]
    pub fn default_types(&self) -> &'db crate::next_solver::DefaultAny<'db> {
        crate::next_solver::default_types()
    }

    #[inline]
    pub(crate) fn expect_crate(&self) -> Crate {
        self.krate.expect("should have a crate")
    }
}

impl<'db> inherent::Span<DbInterner<'db>> for Span {
    fn dummy() -> Self {
        Span::Dummy
    }
}

interned_slice!(
    BoundVarKindsStorage,
    BoundVarKinds,
    StoredBoundVarKinds,
    bound_var_kinds,
    BoundVariableKind<'db>,
    BoundVariableKind<'static>,
);

pub type BoundVariableKind<'db> = rustc_type_ir::BoundVariableKind<DbInterner<'db>>;

impl<'db> inherent::BoundVarKinds<DbInterner<'db>> for BoundVarKinds<'db> {
    #[inline]
    fn from_vars(
        _cx: DbInterner<'db>,
        iter: impl IntoIterator<Item = BoundVariableKind<'db>>,
    ) -> Self {
        BoundVarKinds::new_from_iter(iter)
    }
}

/// rustc's `TypingEnv` as far as the shared IR reads it: the environment a type's layout is
/// computed in.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TypingEnv<'db> {
    pub param_env: ParamEnv<'db>,
}

impl<'db> inherent::TypingEnv<DbInterner<'db>> for TypingEnv<'db> {
    #[inline]
    fn fully_monomorphized() -> Self {
        TypingEnv { param_env: ParamEnv::empty() }
    }

    /// As rustc's `layout_of(..).unwrap().size`: a type the IR asks the size of has a layout.
    fn layout_size(self, interner: DbInterner<'db>, ty: Ty<'db>) -> Size {
        let env =
            crate::ParamEnvAndCrate { param_env: self.param_env, krate: interner.expect_crate() };
        match interner.db.layout_of_ty(ty.store(), env.store()) {
            Ok(layout) => layout.size,
            Err(error) => panic!("the layout of `{ty:?}` is not known: {error:?}"),
        }
    }
}

pub type CanonicalInputData<'db> = rustc_type_ir::solve::CanonicalInputData<DbInterner<'db>>;

/// Every interned [`CanonicalInput`], kept whole: it derefs to its data.
static CANONICAL_INPUTS: table::Values<CanonicalInputData<'static>> =
    table::Values::new("canonical inputs");
table::erase!(CanonicalInputData);

/// A canonical goal, the solver's cache key: its row in [`CANONICAL_INPUTS`], which is never
/// freed, so the search graph hashes and compares a `u32`.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct CanonicalInput<'db> {
    index: NonZeroU32,
    _db: PhantomData<&'db ()>,
}

const _: () = assert!(size_of::<Option<CanonicalInput<'static>>>() == 4);

impl<'db> CanonicalInput<'db> {
    #[inline]
    pub fn new(data: CanonicalInputData<'db>) -> Self {
        CanonicalInput { index: CANONICAL_INPUTS.intern(data), _db: PhantomData }
    }
}

impl<'db> std::ops::Deref for CanonicalInput<'db> {
    type Target = CanonicalInputData<'db>;

    #[inline]
    fn deref(&self) -> &Self::Target {
        CANONICAL_INPUTS.get(self.index)
    }
}

impl fmt::Debug for CanonicalInput<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        (**self).fmt(f)
    }
}

type ParamEnvCache<'db> = CanonicalParamEnvCache<DbInterner<'db>>;
table::erase!(ParamEnvCache);

/// Past this many entries a thread's [`PARAM_ENV_CACHE`] starts over, as nothing is evicted.
const PARAM_ENV_CACHE_ENTRIES: usize = 1 << 14;

thread_local! {
    /// rustc's `canonical_param_env_cache`, one per thread. An entry is a function of its key
    /// alone: a param env with no type or const inference variables, whose free regions all
    /// become placeholders of the root universe, of interned values that are never freed. So
    /// it holds for every interner and database of the process.
    static PARAM_ENV_CACHE: std::cell::Cell<Option<Box<ParamEnvCache<'static>>>> =
        const { std::cell::Cell::new(None) };
}

interned_slice!(
    CanonicalVarsStorage,
    CanonicalVarKinds,
    StoredCanonicalVars,
    canonical_vars,
    CanonicalVarKind<'db>,
    CanonicalVarKind<'static>
);

/// A solved goal's answer, with what solving it read from the database (`None` when it read
/// something no other query may take on, see [`tls_cache`]): a query taking the answer takes
/// the reads too, as rustc's `get_tracked` does.
#[derive(Debug)]
pub struct Tracked<T: fmt::Debug + Clone> {
    value: T,
    reads: Option<salsa::Reads>,
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct AllocId;

interned_slice!(VariancesOfStorage, VariancesOf, StoredVariancesOf, variances, Variance, Variance);

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    struct AdtFlags: u8 {
        const IS_FUNDAMENTAL = 1 << 0;
        const IS_PACKED = 1 << 1;
        const HAS_REPR = 1 << 2;
        const IS_PHANTOM_DATA = 1 << 3;
        const IS_MANUALLY_DROP = 1 << 4;
        const IS_BOX = 1 << 5;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
enum AdtDefInner {
    Struct { id: StructId, flags: AdtFlags },
    Union { id: UnionId, flags: AdtFlags },
    Enum { id: EnumId, flags: AdtFlags },
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct AdtDef(AdtDefInner);

const _: () = assert!(size_of::<AdtDef>() == 12);

impl AdtDef {
    pub fn new<'db>(def_id: AdtId, interner: DbInterner<'db>) -> Self {
        let db = interner.db();
        let inner = match def_id {
            AdtId::StructId(id) => {
                let data = StructSignature::of(db, id);
                let mut flags = AdtFlags::empty();
                if data.flags.contains(StructFlags::FUNDAMENTAL) {
                    flags.insert(AdtFlags::IS_FUNDAMENTAL);
                }
                if data.flags.contains(StructFlags::IS_PHANTOM_DATA) {
                    flags.insert(AdtFlags::IS_PHANTOM_DATA);
                }
                if data.flags.contains(StructFlags::IS_MANUALLY_DROP) {
                    flags.insert(AdtFlags::IS_MANUALLY_DROP);
                }
                if data.flags.contains(StructFlags::IS_BOX) {
                    flags.insert(AdtFlags::IS_BOX);
                }
                if data.flags.contains(StructFlags::HAS_REPR) {
                    flags.insert(AdtFlags::HAS_REPR);
                    if data.repr(db, id).is_some_and(|repr| repr.packed()) {
                        flags.insert(AdtFlags::IS_PACKED);
                    }
                }
                AdtDefInner::Struct { id, flags }
            }
            AdtId::UnionId(id) => {
                let data = UnionSignature::of(db, id);
                let mut flags = AdtFlags::empty();
                if data.flags.contains(StructFlags::FUNDAMENTAL) {
                    flags.insert(AdtFlags::IS_FUNDAMENTAL);
                }
                if data.flags.contains(StructFlags::HAS_REPR) {
                    flags.insert(AdtFlags::HAS_REPR);
                    if data.repr(db, id).is_some_and(|repr| repr.packed()) {
                        flags.insert(AdtFlags::IS_PACKED);
                    }
                }
                AdtDefInner::Union { id, flags }
            }
            AdtId::EnumId(id) => {
                let data = EnumSignature::of(db, id);
                let mut flags = AdtFlags::empty();
                if data.flags.contains(EnumFlags::FUNDAMENTAL) {
                    flags.insert(AdtFlags::IS_FUNDAMENTAL);
                }
                if data.flags.contains(EnumFlags::HAS_REPR) {
                    flags.insert(AdtFlags::HAS_REPR);
                    if data.repr(db, id).is_some_and(|repr| repr.packed()) {
                        flags.insert(AdtFlags::IS_PACKED);
                    }
                }
                AdtDefInner::Enum { id, flags }
            }
        };
        AdtDef(inner)
    }

    #[inline]
    pub fn def_id(self) -> AdtId {
        match self.0 {
            AdtDefInner::Struct { id, .. } => AdtId::StructId(id),
            AdtDefInner::Union { id, .. } => AdtId::UnionId(id),
            AdtDefInner::Enum { id, .. } => AdtId::EnumId(id),
        }
    }

    #[inline]
    fn flags(self) -> AdtFlags {
        match self.0 {
            AdtDefInner::Struct { flags, .. }
            | AdtDefInner::Union { flags, .. }
            | AdtDefInner::Enum { flags, .. } => flags,
        }
    }

    /// The bits of what `new` found of the ADT's definition, for an interned type's row.
    #[inline]
    pub(super) fn flag_bits(self) -> u8 {
        self.flags().bits()
    }

    /// The ADT `new` gave with these flag bits, `None` for bits it never sets.
    #[inline]
    pub(super) fn from_parts(id: AdtId, bits: u8) -> Option<AdtDef> {
        let flags = AdtFlags::from_bits(bits)?;
        Some(AdtDef(match id {
            AdtId::StructId(id) => AdtDefInner::Struct { id, flags },
            AdtId::UnionId(id) => AdtDefInner::Union { id, flags },
            AdtId::EnumId(id) => AdtDefInner::Enum { id, flags },
        }))
    }

    #[inline]
    pub fn is_struct(self) -> bool {
        matches!(self.0, AdtDefInner::Struct { .. })
    }

    #[inline]
    pub fn is_union(self) -> bool {
        matches!(self.0, AdtDefInner::Union { .. })
    }

    #[inline]
    pub fn is_enum(self) -> bool {
        matches!(self.0, AdtDefInner::Enum { .. })
    }

    #[inline]
    pub fn is_box(self) -> bool {
        matches!(self.0, AdtDefInner::Struct { flags, .. } if flags.contains(AdtFlags::IS_BOX))
    }

    #[inline]
    pub fn repr(self, db: &dyn HirDatabase) -> ReprOptions {
        if self.flags().contains(AdtFlags::HAS_REPR) {
            AttrFlags::repr_assume_has(db, self.def_id()).unwrap_or_default()
        } else {
            ReprOptions::default()
        }
    }
}

impl<'db> inherent::AdtDef<DbInterner<'db>> for AdtDef {
    fn def_id(self) -> AdtIdWrapper {
        self.def_id().into()
    }

    fn is_struct(self) -> bool {
        self.is_struct()
    }

    fn is_phantom_data(self) -> bool {
        matches!(self.0, AdtDefInner::Struct { flags, .. } if flags.contains(AdtFlags::IS_PHANTOM_DATA))
    }

    fn is_manually_drop(self) -> bool {
        matches!(self.0, AdtDefInner::Struct { flags, .. } if flags.contains(AdtFlags::IS_MANUALLY_DROP))
    }

    fn is_packed(self) -> bool {
        self.flags().contains(AdtFlags::IS_PACKED)
    }

    fn is_fundamental(self) -> bool {
        self.flags().contains(AdtFlags::IS_FUNDAMENTAL)
    }

    fn struct_tail_ty(
        self,
        interner: DbInterner<'db>,
    ) -> Option<EarlyBinder<DbInterner<'db>, Ty<'db>>> {
        let hir_def::AdtId::StructId(struct_id) = self.def_id() else {
            return None;
        };
        let id: VariantId = struct_id.into();
        let field_types = interner.db().field_types(id);

        field_types.iter().last().map(|f| f.1.ty())
    }

    fn all_field_tys(
        self,
        interner: DbInterner<'db>,
    ) -> EarlyBinder<DbInterner<'db>, impl IntoIterator<Item = Ty<'db>>> {
        let db = interner.db();
        let field_tys =
            |id: VariantId| db.field_types(id).iter().map(|(_, ty)| ty.ty().skip_binder());
        let tys = match self.def_id() {
            hir_def::AdtId::StructId(id) => Either::Left(field_tys(id.into())),
            hir_def::AdtId::UnionId(id) => Either::Left(field_tys(id.into())),
            hir_def::AdtId::EnumId(id) => Either::Right(
                id.enum_variants(db)
                    .variants
                    .values()
                    .flat_map(move |&(variant_id, _)| field_tys(variant_id.into())),
            ),
        };

        // The field types are read out of stored binders, which hold no rigid alias.
        EarlyBinder::bind_unchecked(tys)
    }

    fn sizedness_constraint(
        self,
        interner: DbInterner<'db>,
        sizedness: SizedTraitKind,
    ) -> Option<EarlyBinder<DbInterner<'db>, Ty<'db>>> {
        let tail_ty = self.struct_tail_ty(interner)?;
        tail_ty
            .map_bound(|tail_ty| sizedness_constraint_for_ty(interner, sizedness, tail_ty))
            .transpose()
    }

    fn destructor(self, interner: DbInterner<'db>) -> Option<AdtDestructorKind> {
        crate::effects::destructor_is_const(interner.db, self.def_id()).map(|is_const| {
            if is_const { AdtDestructorKind::Const } else { AdtDestructorKind::NotConst }
        })
    }

    fn field_representing_type_info(
        self,
        _interner: DbInterner<'db>,
        _args: GenericArgs<'db>,
    ) -> Option<rustc_type_ir::FieldInfo<DbInterner<'db>>> {
        // FIXME
        None
    }
}

impl fmt::Debug for AdtDef {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        crate::with_attached_db(|db| match self.0 {
            AdtDefInner::Struct { id, .. } => {
                let data = StructSignature::of(db, id);
                f.write_str(data.name.as_str())
            }
            AdtDefInner::Union { id, .. } => {
                let data = UnionSignature::of(db, id);
                f.write_str(data.name.as_str())
            }
            AdtDefInner::Enum { id, .. } => {
                let data = EnumSignature::of(db, id);
                f.write_str(data.name.as_str())
            }
        })
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub struct Features;

impl<'db> inherent::Features<DbInterner<'db>> for Features {
    fn generic_const_exprs(self) -> bool {
        false
    }

    fn coroutine_clone(self) -> bool {
        false
    }

    fn gca_const_items(self) -> bool {
        false
    }

    fn feature_bound_holds_in_crate(self, _symbol: Symbol) -> bool {
        false
    }
}

/// A name, as `rustc_span::Symbol`: an item's (see [`Interner::item_name`]) or a keyword.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct Symbol(pub intern::Symbol);

const _: () = assert!(size_of::<Symbol>() == size_of::<intern::Symbol>());

// SAFETY: a symbol holds no interned type, so there is nothing to visit.
unsafe impl<V> GenericTypeVisitable<V> for Symbol {
    #[inline]
    fn generic_visit_with(&self, _visitor: &mut V) {}
}

impl<'db> inherent::Symbol<DbInterner<'db>> for Symbol {
    const KW_UNDERSCORE_LIFETIME: Self = Symbol(sym::tick_underscore);
    const KW_STATIC_LIFETIME: Self = Symbol(sym::tick_static);
    const SYM_ANON: Self = Symbol(sym::anon);
}

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct UnsizingParams(pub(crate) DenseBitSet<u32>);

impl std::ops::Deref for UnsizingParams {
    type Target = DenseBitSet<u32>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub type PatternKind<'db> = rustc_type_ir::PatternKind<DbInterner<'db>>;

/// Every interned [`Pattern`].
static PATTERNS: table::Rows<PatternKind<'static>> = table::Rows::new("patterns", table::LAST_ROW);
table::erase!(PatternKind);

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Pattern<'db> {
    index: NonZeroU32,
    _db: PhantomData<&'db ()>,
}

const _: () = assert!(size_of::<Option<Pattern<'static>>>() == 4);
// A row costs `table::ROW_BYTES` hot bytes and its kind, at most 48 cold ones.
const _: () = assert!(size_of::<PatternKind<'static>>() <= 48);

impl<'db> Pattern<'db> {
    pub fn new(_interner: DbInterner<'db>, kind: PatternKind<'db>) -> Self {
        Self::intern(kind)
    }

    #[inline]
    pub(super) fn intern(kind: PatternKind<'db>) -> Self {
        Self::at(PATTERNS.intern(kind, || {
            let (flags, binder) = pattern_flags(kind);
            table::RowInfo { tag: persist::tag(&kind), flags, binder }
        }))
    }

    #[inline]
    pub(super) fn at(index: NonZeroU32) -> Self {
        Pattern { index, _db: PhantomData }
    }

    #[inline]
    pub(super) fn row(self) -> NonZeroU32 {
        self.index
    }
}

/// The flags and outer exclusive binder of a pattern: those of the constants in it.
fn pattern_flags(kind: PatternKind<'_>) -> (TypeFlags, rustc_type_ir::DebruijnIndex) {
    match kind {
        PatternKind::Range { start, end } => (
            start.flags() | end.flags(),
            start.outer_exclusive_binder().max(end.outer_exclusive_binder()),
        ),
        PatternKind::Or(pats) => pats.iter().fold(
            (TypeFlags::empty(), rustc_type_ir::INNERMOST),
            |(flags, binder), pat| {
                (flags | pat.flags(), binder.max(pat.outer_exclusive_binder()))
            },
        ),
        PatternKind::NotNull => (TypeFlags::empty(), rustc_type_ir::INNERMOST),
    }
}

impl<'db> std::fmt::Debug for Pattern<'db> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.kind().fmt(f)
    }
}

impl<'db> Flags for Pattern<'db> {
    #[inline]
    fn flags(&self) -> TypeFlags {
        PATTERNS.flags(self.index)
    }

    #[inline]
    fn outer_exclusive_binder(&self) -> rustc_type_ir::DebruijnIndex {
        PATTERNS.binder(self.index)
    }
}

impl<'db> rustc_type_ir::inherent::IntoKind for Pattern<'db> {
    type Kind = rustc_type_ir::PatternKind<DbInterner<'db>>;
    #[inline]
    fn kind(self) -> Self::Kind {
        PATTERNS.kind(self.index)
    }
}

impl<'db> rustc_type_ir::TypeVisitable<DbInterner<'db>> for Pattern<'db> {
    fn visit_with<V: rustc_type_ir::TypeVisitor<DbInterner<'db>>>(
        &self,
        visitor: &mut V,
    ) -> V::Result {
        self.kind().visit_with(visitor)
    }
}

impl<'db, V: WorldExposer> rustc_type_ir::GenericTypeVisitable<V> for Pattern<'db> {
    fn generic_visit_with(&self, visitor: &mut V) {
        if visitor.on_interned(PATTERNS.name(), self.index.get()).is_continue() {
            self.kind().generic_visit_with(visitor);
        }
    }
}

impl<'db> rustc_type_ir::TypeFoldable<DbInterner<'db>> for Pattern<'db> {
    fn try_fold_with<F: rustc_type_ir::FallibleTypeFolder<DbInterner<'db>>>(
        self,
        folder: &mut F,
    ) -> Result<Self, F::Error> {
        Ok(Pattern::new(folder.cx(), self.kind().try_fold_with(folder)?))
    }

    fn fold_with<F: rustc_type_ir::TypeFolder<DbInterner<'db>>>(self, folder: &mut F) -> Self {
        Pattern::new(folder.cx(), self.kind().fold_with(folder))
    }
}

impl<'db> rustc_type_ir::relate::Relate<DbInterner<'db>> for Pattern<'db> {
    fn relate<R: rustc_type_ir::relate::TypeRelation<DbInterner<'db>>>(
        relation: &mut R,
        a: Self,
        b: Self,
    ) -> rustc_type_ir::relate::RelateResult<DbInterner<'db>, Self> {
        let tcx = relation.cx();
        match (a.kind(), b.kind()) {
            (
                PatternKind::Range { start: start_a, end: end_a },
                PatternKind::Range { start: start_b, end: end_b },
            ) => {
                let start = relation.relate(start_a, start_b)?;
                let end = relation.relate(end_a, end_b)?;
                Ok(Pattern::new(tcx, PatternKind::Range { start, end }))
            }
            (PatternKind::Or(a), PatternKind::Or(b)) => {
                if a.len() != b.len() {
                    return Err(TypeError::Mismatch);
                }
                let pats = PatList::new_from_iter(
                    std::iter::zip(a.iter(), b.iter()).map(|(a, b)| relation.relate(a, b)),
                )?;
                Ok(Pattern::new(tcx, PatternKind::Or(pats)))
            }
            (PatternKind::NotNull, PatternKind::NotNull) => Ok(a),
            (PatternKind::Range { .. } | PatternKind::Or(_) | PatternKind::NotNull, _) => {
                Err(TypeError::Mismatch)
            }
        }
    }
}

interned_slice!(PatListStorage, PatList, StoredPatList, pat_list, Pattern<'db>, Pattern<'static>);
impl_foldable_for_interned_slice!(PatList);

macro_rules! as_lang_item {
    (
        $solver_enum:ident, $self:ident, $def_id:expr, $id_ty:ty;

        $( $variant:ident ),* $(,)?
    ) => {{
        let lang_items = $self.lang_items();
        // Ensure exhaustiveness.
        if let Some(it) = None::<$solver_enum> {
            match it {
                $( $solver_enum::$variant => {} )*
            }
        }
        match $def_id {
            $( def_id if let Some(it) = lang_items.$variant && <$id_ty>::from(it) == def_id => Some($solver_enum::$variant), )*
            _ => None
        }
    }};
}

macro_rules! is_lang_item {
    (
        $solver_enum:ident, $self:ident, $def_id:expr, $expected_variant:ident;

        $( $variant:ident ),* $(,)?
    ) => {{
        let lang_items = $self.lang_items();
        let def_id = $def_id;
        match $expected_variant {
            $( $solver_enum::$variant => lang_items.$variant.is_some_and(|it| it == def_id), )*
        }
    }};
}

impl<'db> Interner for DbInterner<'db> {
    type DefId = SolverDefId<'db>;
    type LocalDefId = SolverDefId<'db>;
    type LocalDefIds = SolverDefIds<'db>;
    type TraitId = TraitIdWrapper;
    type ForeignId = TypeAliasIdWrapper;
    type FunctionId = CallableIdWrapper;
    type ClosureId = ClosureIdWrapper<'db>;
    type CoroutineClosureId = CoroutineClosureIdWrapper<'db>;
    type CoroutineId = CoroutineIdWrapper<'db>;
    type AdtId = AdtIdWrapper;
    type ImplId = AnyImplId;
    type AnonConstId = AnonConstIdWrapper<'db>;
    type TraitAssocTyId = TraitAssocTyId;
    type TraitAssocConstId = TraitAssocConstId;
    type TraitAssocTermId = TraitAssocTermId;
    type OpaqueTyId = OpaqueTyIdWrapper<'db>;
    type LocalOpaqueTyId = OpaqueTyIdWrapper<'db>;
    type FreeTyAliasId = FreeTyAliasId;
    type FreeConstAliasId = FreeConstAliasId;
    type FreeTermAliasId = FreeTermAliasId;
    type ImplOrTraitAssocTyId = ImplOrTraitAssocTyId;
    type ImplOrTraitAssocConstId = ImplOrTraitAssocConstId;
    type ImplOrTraitAssocTermId = ImplOrTraitAssocTermId;
    type InherentAssocTyId = InherentAssocTyId;
    type InherentAssocConstId = InherentAssocConstId;
    type InherentAssocTermId = InherentAssocTermId;
    type Span = Span;

    type GenericArgs = GenericArgs<'db>;
    type GenericArgsSlice = &'db [GenericArg<'db>];
    type GenericArg = GenericArg<'db>;

    type Term = Term<'db>;

    type BoundVarKinds = BoundVarKinds<'db>;
    type TypingEnv = TypingEnv<'db>;

    type PredefinedOpaques = PredefinedOpaques<'db>;

    fn mk_predefined_opaques_in_body(
        self,
        data: &[(OpaqueTypeKey<'db>, Self::Ty)],
    ) -> Self::PredefinedOpaques {
        PredefinedOpaques::new_from_slice(data)
    }

    type CanonicalVarKinds = CanonicalVarKinds<'db>;

    fn mk_canonical_var_kinds(
        self,
        kinds: &[rustc_type_ir::CanonicalVarKind<Self>],
    ) -> Self::CanonicalVarKinds {
        CanonicalVarKinds::new_from_slice(kinds)
    }

    type ExternalConstraints = ExternalConstraints<'db>;

    fn mk_external_constraints(
        self,
        data: rustc_type_ir::solve::ExternalConstraintsData<Self>,
    ) -> Self::ExternalConstraints {
        ExternalConstraints::new(self, data)
    }

    type DepNodeIndex = Option<salsa::Reads>;

    type Tracked<T: fmt::Debug + Clone> = Tracked<T>;

    type Ty = Ty<'db>;
    type Tys = Tys<'db>;
    type FnInputTys = &'db [Ty<'db>];
    type ParamTy = ParamTy;
    type Symbol = Symbol;

    type ErrorGuaranteed = ErrorGuaranteed;
    type BoundExistentialPredicates = BoundExistentialPredicates<'db>;
    type AllocId = AllocId;
    type Pat = Pattern<'db>;
    type PatList = PatList<'db>;
    type Safety = Safety;

    type ParamConst = ParamConst;
    type ValueConst = ValueConst<'db>;
    type ValTree = ValTree<'db>;
    type Consts = Consts<'db>;
    type ScalarInt = ScalarInt;
    type Scalar = Scalar;
    type ExprConst = ExprConst;

    type EarlyParamRegion = EarlyParamRegion;
    type LateParamRegionKind = LateParamRegionKind<'db>;

    type InternedRegionKind = InternedRegion<'db>;
    type InternedConstKind = InternedConst<'db>;

    type RegionAssumptions = RegionAssumptions<'db>;

    type ParamEnv = ParamEnv<'db>;
    type Predicate = Predicate<'db>;
    type Clause = Clause<'db>;
    type Clauses = Clauses<'db>;

    type GenericsOf = Generics<'db>;

    type VariancesOf = VariancesOf<'db>;

    type AdtDef = AdtDef;

    type Features = Features;

    fn mk_args(self, args: &[Self::GenericArg]) -> Self::GenericArgs {
        GenericArgs::new_from_slice(args)
    }

    fn mk_args_from_iter<I, T>(self, args: I) -> T::Output
    where
        I: Iterator<Item = T>,
        T: rustc_type_ir::CollectAndApply<Self::GenericArg, Self::GenericArgs>,
    {
        GenericArgs::new_from_iter(args)
    }

    type UnsizingParams = UnsizingParams;

    fn mk_tracked<T: fmt::Debug + Clone>(
        self,
        data: T,
        reads: Self::DepNodeIndex,
    ) -> Self::Tracked<T> {
        Tracked { value: data, reads }
    }

    fn get_tracked<T: fmt::Debug + Clone>(self, tracked: &Self::Tracked<T>) -> T {
        match tracked.reads {
            // From the shared cache: the query taking it reruns when the reads change, and so
            // does any goal it is solving in a capture of its own, which takes them too.
            Some(reads) => salsa::replay_reads(self.db, reads),
            // From this query's own cache: the query made the reads itself, but a goal being
            // captured now did not, and so must not be shared either.
            None => salsa::unshareable_reads(self.db),
        }
        tracked.value.clone()
    }

    fn with_cached_task<T>(self, task: impl FnOnce() -> T) -> (T, Self::DepNodeIndex) {
        // Every goal the solver computes runs in here, a nested goal inside its parent's. A
        // goal can take minutes of solving that asks the database nothing, so a cancelled one
        // is noticed here rather than at the next query.
        salsa::Database::unwind_if_revision_cancelled(self.db);
        salsa::capture_reads(self.db, || super::ensure_sufficient_stack(task))
    }

    fn cache_scope(self, reads: &Self::DepNodeIndex) -> rustc_type_ir::search_graph::CacheScope {
        // A goal is shared only with its reads, which `capture_reads` gives only when any
        // query may replay them.
        match reads {
            Some(_) => rustc_type_ir::search_graph::CacheScope::Shared,
            None => rustc_type_ir::search_graph::CacheScope::Query,
        }
    }

    fn with_global_cache<R>(
        self,
        scope: rustc_type_ir::search_graph::CacheScope,
        f: impl FnOnce(&rustc_type_ir::search_graph::GlobalCache<Self>) -> R,
    ) -> R {
        match scope {
            rustc_type_ir::search_graph::CacheScope::Shared => {
                tls_cache::shared(self.db, self.krate, self.regions, f)
            }
            rustc_type_ir::search_graph::CacheScope::Query => {
                tls_cache::of_query(self.db, self.solved, f)
            }
        }
    }

    fn with_canonical_param_env_cache<R>(
        self,
        f: impl FnOnce(&mut CanonicalParamEnvCache<Self>) -> R,
    ) -> R {
        with_param_env_cache(f)
    }

    fn assert_evaluation_is_concurrent(&self) {
        // Turns out, the assumption for this function isn't perfect.
        // See https://github.com/rust-lang/trait-system-refactor-initiative/issues/234
    }

    fn expand_abstract_consts<T: rustc_type_ir::TypeFoldable<Self>>(self, _: T) -> T {
        unreachable!("only used by the old trait solver in rustc");
    }

    fn generics_of(self, def_id: Self::DefId) -> Self::GenericsOf {
        generics(self, def_id)
    }

    fn variances_of(self, def_id: Self::DefId) -> Self::VariancesOf {
        let generic_def = match def_id {
            // A constructor's variances come from its signature, not its type's.
            SolverDefId::Ctor(Ctor::Enum(def_id)) => {
                return crate::variance::ctor_variances(self.db, def_id.into());
            }
            SolverDefId::Ctor(Ctor::Struct(def_id)) => {
                return crate::variance::ctor_variances(self.db, def_id.into());
            }
            SolverDefId::EnumVariantId(def_id) => def_id.loc(self.db).parent.into(),
            SolverDefId::InternedOpaqueTyId(_def_id) => {
                // FIXME(next-solver): track variances
                //
                // We compute them based on the only `Ty` level info in rustc,
                // move `variances_of_opaque` into `rustc_next_trait_solver` for reuse.
                return VariancesOf::new_from_iter(
                    (0..self.generics_of(def_id).count()).map(|_| Variance::Invariant),
                );
            }
            SolverDefId::AdtId(def_id) => def_id.into(),
            SolverDefId::FunctionId(def_id) => def_id.into(),
            SolverDefId::ConstId(_)
            | SolverDefId::StaticId(_)
            | SolverDefId::TraitId(_)
            | SolverDefId::TypeAliasId(_)
            | SolverDefId::ImplId(_)
            | SolverDefId::BuiltinDeriveImplId(_)
            | SolverDefId::InternedClosureId(_)
            | SolverDefId::InternedCoroutineId(_)
            | SolverDefId::InternedCoroutineClosureId(_)
            | SolverDefId::AnonConstId(_) => {
                return VariancesOf::empty();
            }
        };
        self.db.variances_of(generic_def)
    }

    fn type_of(self, def_id: Self::DefId) -> EarlyBinder<Self, Self::Ty> {
        match def_id {
            SolverDefId::TypeAliasId(id) => self.db().ty(id.into()),
            SolverDefId::AdtId(id) => self.db().ty(id.into()),
            // An inherent alias's self type is equated with its impl's.
            SolverDefId::ImplId(id) => self.db.impl_self_ty(id),
            SolverDefId::BuiltinDeriveImplId(id) => crate::builtin_derive::impl_trait(self, id)
                .map_bound(|trait_ref| trait_ref.self_ty()),
            // `AliasConst::type_of`: a const's type, and an anonymous const's.
            SolverDefId::ConstId(id) => self.db.value_ty(id.into()).unwrap(),
            SolverDefId::StaticId(id) => self.db.value_ty(id.into()).unwrap(),
            SolverDefId::AnonConstId(id) => id.loc(self.db).ty.get(),
            // FIXME(next-solver): This uses the types of `query mir_borrowck` in rustc.
            //
            // We currently always use the type from HIR typeck which ignores regions. This
            // should be fine.
            SolverDefId::InternedOpaqueTyId(def_id) => {
                self.type_of_opaque_hir_typeck(def_id.into())
            }
            SolverDefId::FunctionId(id) => self.db.value_ty(id.into()).unwrap(),
            SolverDefId::Ctor(id) => {
                let id = match id {
                    Ctor::Struct(id) => id.into(),
                    Ctor::Enum(id) => id.into(),
                };
                self.db.value_ty(id).expect("`SolverDefId::Ctor` should have a function-like ctor")
            }
            _ => panic!("Unexpected def_id `{def_id:?}` provided for `type_of`"),
        }
    }

    fn adt_def(self, def_id: Self::AdtId) -> Self::AdtDef {
        AdtDef::new(def_id.0, self)
    }

    /// hir-ty's spans name expressions and types within a body, none an item.
    fn def_span(self, _def_id: Self::DefId) -> Self::Span {
        Span::Dummy
    }

    /// No item is a `type const` (`min_generic_const_args`): every alias const is evaluated.
    fn is_direct_const(self, _alias: AliasConstKind<Self>) -> bool {
        false
    }

    /// `None`, so the solver evaluates the const (`evaluate_const`), as rustc does for a const
    /// item that is no `type const`.
    fn const_of_item(
        self,
        _alias: AliasConstKind<Self>,
    ) -> Option<EarlyBinder<Self, rustc_type_ir::Const<Self>>> {
        None
    }

    fn mk_ct_from_kind(self, kind: ConstKind<Self>) -> rustc_type_ir::Const<Self> {
        rustc_type_ir::Const(InternedConst::intern(kind))
    }

    /// rustc's `-Zassumptions-on-binders`, off.
    fn assumptions_on_binders(self) -> bool {
        false
    }

    /// rustc's check that a rigid alias stays rigid when normalized again, off.
    fn renormalize_rigid_aliases(self) -> bool {
        false
    }

    fn trait_ref_and_own_args_for_alias(
        self,
        def_id: Self::TraitAssocTermId,
        args: Self::GenericArgs,
    ) -> (rustc_type_ir::TraitRef<Self>, Self::GenericArgsSlice) {
        let trait_def_id = self.projection_parent(def_id);
        let trait_ref = TraitRef::from_assoc(self, trait_def_id, args);
        (trait_ref, &args.as_slice()[trait_ref.args.len()..])
    }

    /// An inherent alias in "Self form" (`InherentTy`, `InherentConstSelf`) has the self type
    /// and then the item's own parameters; any other has its item's every parameter.
    fn check_alias_term_args_compatible(
        self,
        term_kind: AliasTermKind<'db>,
        args: Self::GenericArgs,
    ) -> bool {
        let generics = self.generics_of(alias_term_def_id(term_kind));
        match term_kind {
            AliasTermKind::InherentTy { .. } | AliasTermKind::InherentConstSelf { .. } => {
                match args.as_slice() {
                    [self_ty, own @ ..] => {
                        matches!(self_ty.kind(), GenericArgKind::Type(_))
                            && args_fit_params(generics.iter_own(), own)
                    }
                    [] => false,
                }
            }
            _ => args_fit_params(generics.iter().map(|(param, _)| param), args.as_slice()),
        }
    }

    fn debug_assert_alias_term_args_compatible(
        self,
        term_kind: AliasTermKind<'db>,
        args: Self::GenericArgs,
    ) {
        if cfg!(debug_assertions) && !self.check_alias_term_args_compatible(term_kind, args) {
            panic!("args {args:?} do not fit the alias {term_kind:?}");
        }
    }

    fn debug_assert_args_compatible(self, _def_id: Self::DefId, _args: Self::GenericArgs) {}

    fn debug_assert_existential_args_compatible(
        self,
        _def_id: Self::DefId,
        _args: Self::GenericArgs,
    ) {
    }

    fn mk_type_list_from_iter<I, T>(self, args: I) -> T::Output
    where
        I: Iterator<Item = T>,
        T: rustc_type_ir::CollectAndApply<Self::Ty, Self::Tys>,
    {
        Tys::new_from_iter(args)
    }

    fn projection_parent(self, def_id: Self::TraitAssocTermId) -> Self::TraitId {
        let container = match def_id.0 {
            TermId::TypeAliasId(def_id) => def_id.loc(self.db).container,
            TermId::ConstId(def_id) => def_id.loc(self.db).container,
        };
        let ItemContainerId::TraitId(trait_) = container else {
            panic!("a TraitAssocTermId can only come from a trait")
        };
        trait_.into()
    }

    fn impl_or_trait_assoc_term_parent(self, def_id: Self::ImplOrTraitAssocTermId) -> Self::DefId {
        let container = match def_id.0 {
            TermId::TypeAliasId(def_id) => def_id.loc(self.db).container,
            TermId::ConstId(def_id) => def_id.loc(self.db).container,
        };
        match container {
            ItemContainerId::ImplId(impl_) => impl_.into(),
            ItemContainerId::TraitId(trait_) => trait_.into(),
            ItemContainerId::ExternBlockId(_) | ItemContainerId::ModuleId(_) => {
                panic!("only impl or trait can be the parent of ImplOrTraitAssocTermId")
            }
        }
    }

    fn inherent_alias_term_parent(self, def_id: Self::InherentAssocTermId) -> Self::ImplId {
        let container = match def_id.0 {
            TermId::TypeAliasId(def_id) => def_id.loc(self.db).container,
            TermId::ConstId(def_id) => def_id.loc(self.db).container,
        };
        match container {
            ItemContainerId::ImplId(impl_) => impl_.into(),
            ItemContainerId::ExternBlockId(_)
            | ItemContainerId::ModuleId(_)
            | ItemContainerId::TraitId(_) => {
                panic!("only impl can be the parent of InherentAliasTermId")
            }
        }
    }

    fn recursion_limit(self) -> usize {
        // A lower limit than the crate's makes deeply nested but valid obligations (tower's
        // future types) ambiguous.
        self.recursion_limit as usize
    }

    fn features(self) -> Features {
        Features
    }

    fn fn_sig(
        self,
        def_id: Self::FunctionId,
    ) -> EarlyBinder<Self, rustc_type_ir::Binder<Self, rustc_type_ir::FnSig<Self>>> {
        self.db().callable_item_signature(def_id.0)
    }

    fn coroutine_movability(self, def_id: Self::CoroutineId) -> rustc_ast_ir::Movability {
        match def_id.0.loc(self.db).kind {
            hir_def::hir::ClosureKind::OldCoroutine(movability) => match movability {
                hir_def::hir::Movability::Static => rustc_ast_ir::Movability::Static,
                hir_def::hir::Movability::Movable => rustc_ast_ir::Movability::Movable,
            },
            hir_def::hir::ClosureKind::Coroutine { .. } => rustc_ast_ir::Movability::Static,
            kind => panic!("unexpected kind for a coroutine: {kind:?}"),
        }
    }

    fn coroutine_for_closure(self, def_id: Self::CoroutineClosureId) -> Self::CoroutineId {
        let InternedClosure { owner, expr: coroutine_closure_expr, kind: coroutine_closure_kind } =
            def_id.0.loc(self.db);
        let coroutine_closure_kind = match coroutine_closure_kind {
            HirClosureKind::CoroutineClosure(it) => it,
            _ => {
                panic!("invalid kind closure kind {coroutine_closure_kind:?} for coroutine closure")
            }
        };
        let coroutine_expr = ExpressionStore::coroutine_for_closure(coroutine_closure_expr);
        let coroutine_kind = hir_def::hir::ClosureKind::Coroutine {
            kind: coroutine_closure_kind,
            source: hir_def::hir::CoroutineSource::Closure,
        };
        InternedCoroutineId::new(
            self.db,
            InternedClosure { owner, expr: coroutine_expr, kind: coroutine_kind },
        )
        .into()
    }

    fn generics_require_sized_self(self, def_id: Self::DefId) -> bool {
        match GenericDefId::try_from(def_id) {
            Ok(def_id) => crate::dyn_compatibility::generics_require_sized_self(self.db, def_id),
            Err(_) => false,
        }
    }

    #[tracing::instrument(skip(self))]
    fn item_bounds(
        self,
        def_id: Self::DefId,
    ) -> EarlyBinder<Self, impl IntoIterator<Item = Self::Clause>> {
        ItemBounds::of_solver_def(self.db, def_id).all_bounds()
    }

    #[tracing::instrument(skip(self))]
    fn item_self_bounds(
        self,
        def_id: Self::DefId,
    ) -> EarlyBinder<Self, impl IntoIterator<Item = Self::Clause>> {
        ItemBounds::of_solver_def(self.db, def_id).self_bounds()
    }

    fn item_non_self_bounds(
        self,
        def_id: Self::DefId,
    ) -> EarlyBinder<Self, impl IntoIterator<Item = Self::Clause>> {
        ItemBounds::of_solver_def(self.db, def_id).non_self_bounds()
    }

    fn clauses_of(
        self,
        def_id: Self::DefId,
    ) -> EarlyBinder<Self, impl IntoIterator<Item = Self::Clause>> {
        generic_predicates_of(self.db, def_id).all_predicates()
    }

    fn own_clauses_of(
        self,
        def_id: Self::DefId,
    ) -> EarlyBinder<Self, impl IntoIterator<Item = Self::Clause>> {
        generic_predicates_of(self.db, def_id).own_explicit_predicates()
    }

    fn explicit_super_clauses_of(
        self,
        def_id: Self::TraitId,
    ) -> EarlyBinder<Self, impl IntoIterator<Item = (Self::Clause, Self::Span)>> {
        GenericPredicates::query(self.db, def_id.0.into())
            .explicit_non_assoc_types_predicates()
            .map_bound(move |predicates| {
                predicates.filter(|p| is_clause_at_ty(p, is_ty_self)).map(|p| (p, Span::dummy()))
            })
    }

    fn explicit_implied_clauses_of(
        self,
        def_id: Self::DefId,
    ) -> EarlyBinder<Self, impl IntoIterator<Item = (Self::Clause, Self::Span)>> {
        fn is_ty_assoc_of_self(ty: Ty<'_>) -> bool {
            // FIXME: Is this correct wrt. combined kind of assoc type bounds, i.e. `where Self::Assoc: Trait<Assoc2: Trait>`
            // wrt. `Assoc2`, which we should exclude?
            match ty.kind() {
                TyKind::Alias(_, alias) if alias.kind.try_to_projection().is_some() => {
                    is_ty_assoc_of_self(alias.self_ty())
                }
                _ => is_ty_self(ty),
            }
        }

        let predicates = generic_predicates_of(self.db, def_id);
        let non_assoc_types = predicates
            .explicit_non_assoc_types_predicates()
            .skip_binder()
            .filter(|p| is_clause_at_ty(p, is_ty_self));
        let assoc_types = predicates
            .explicit_assoc_types_predicates()
            .skip_binder()
            .filter(|p| is_clause_at_ty(p, is_ty_assoc_of_self));
        // Both halves are read out of one stored binder, which holds no rigid alias.
        EarlyBinder::bind_unchecked(
            non_assoc_types.chain(assoc_types).map(|it| (it, Span::dummy())),
        )
    }

    fn impl_super_outlives(
        self,
        impl_id: Self::ImplId,
    ) -> EarlyBinder<Self, impl IntoIterator<Item = Self::Clause>> {
        let bounds = match impl_id {
            AnyImplId::ImplId(id) => {
                impl_super_outlives(self.db, id).get().map_bound(|it| it.as_slice())
            }
            AnyImplId::BuiltinDeriveImplId(id) => {
                crate::builtin_derive::impl_super_outlives(self, id)
            }
        };
        bounds.map_bound(|it| it.iter().copied())
    }

    fn const_conditions(
        self,
        def_id: Self::DefId,
    ) -> EarlyBinder<
        Self,
        impl IntoIterator<Item = rustc_type_ir::Binder<Self, rustc_type_ir::TraitRef<Self>>>,
    > {
        let db = self.db();
        let of = |def: GenericDefId| {
            crate::effects::effect_trait_refs(
                crate::lower::const_conditions(db, def).skip_binder().iter().copied(),
            )
            .collect()
        };
        EarlyBinder::bind(self, match def_id {
            SolverDefId::FunctionId(it) => of(it.into()),
            SolverDefId::ImplId(it) => of(it.into()),
            SolverDefId::TraitId(it) => of(it.into()),
            SolverDefId::TypeAliasId(it) => of(it.into()),
            SolverDefId::InternedClosureId(closure)
                if crate::effects::closure_is_const(db, closure) =>
            {
                of(closure.loc(db).owner.generic_def(db))
            }
            SolverDefId::BuiltinDeriveImplId(it) => {
                crate::effects::builtin_derive_const_conditions(db, it)
            }
            _ => Vec::new(),
        })
    }

    fn has_target_features(self, _def_id: Self::FunctionId) -> bool {
        false
    }

    fn require_projection_lang_item(
        self,
        lang_item: SolverProjectionLangItem,
    ) -> Self::TraitAssocTyId {
        let lang_items = self.lang_items();
        let lang_item = match lang_item {
            SolverProjectionLangItem::AsyncFnKindUpvars => lang_items.AsyncFnKindUpvars,
            SolverProjectionLangItem::AsyncFnOnceOutput => lang_items.AsyncFnOnceOutput,
            SolverProjectionLangItem::CallOnceFuture => lang_items.CallOnceFuture,
            SolverProjectionLangItem::CallRefFuture => lang_items.CallRefFuture,
            SolverProjectionLangItem::CoroutineReturn => lang_items.CoroutineReturn,
            SolverProjectionLangItem::CoroutineYield => lang_items.CoroutineYield,
            SolverProjectionLangItem::FutureOutput => lang_items.FutureOutput,
            SolverProjectionLangItem::Metadata => lang_items.Metadata,
            SolverProjectionLangItem::FieldBase => lang_items.FieldBase,
            SolverProjectionLangItem::FieldType => lang_items.FieldType,
        };
        lang_item.expect("Lang item required but not found.").into()
    }

    fn require_trait_lang_item(self, lang_item: SolverTraitLangItem) -> TraitIdWrapper {
        let lang_items = self.lang_items();
        let lang_item = match lang_item {
            SolverTraitLangItem::AsyncFn => lang_items.AsyncFn,
            SolverTraitLangItem::AsyncFnKindHelper => lang_items.AsyncFnKindHelper,
            SolverTraitLangItem::AsyncFnMut => lang_items.AsyncFnMut,
            SolverTraitLangItem::AsyncFnOnce => lang_items.AsyncFnOnce,
            SolverTraitLangItem::AsyncIterator => lang_items.AsyncIterator,
            SolverTraitLangItem::Clone => lang_items.Clone,
            SolverTraitLangItem::Copy => lang_items.Copy,
            SolverTraitLangItem::Coroutine => lang_items.Coroutine,
            SolverTraitLangItem::Destruct => lang_items.Destruct,
            SolverTraitLangItem::DiscriminantKind => lang_items.DiscriminantKind,
            SolverTraitLangItem::Drop => lang_items.Drop,
            SolverTraitLangItem::Fn => lang_items.Fn,
            SolverTraitLangItem::FnMut => lang_items.FnMut,
            SolverTraitLangItem::FnOnce => lang_items.FnOnce,
            SolverTraitLangItem::FnPtrTrait => lang_items.FnPtrTrait,
            SolverTraitLangItem::FusedIterator => lang_items.FusedIterator,
            SolverTraitLangItem::Future => lang_items.Future,
            SolverTraitLangItem::Iterator => lang_items.Iterator,
            SolverTraitLangItem::PointeeTrait => lang_items.PointeeTrait,
            SolverTraitLangItem::Sized => lang_items.Sized,
            SolverTraitLangItem::MetaSized => lang_items.MetaSized,
            SolverTraitLangItem::PointeeSized => lang_items.PointeeSized,
            SolverTraitLangItem::TransmuteTrait => lang_items.TransmuteTrait,
            SolverTraitLangItem::Tuple => lang_items.Tuple,
            SolverTraitLangItem::Unpin => lang_items.Unpin,
            SolverTraitLangItem::Unsize => lang_items.Unsize,
            SolverTraitLangItem::BikeshedGuaranteedNoDrop => lang_items.BikeshedGuaranteedNoDrop,
            SolverTraitLangItem::TrivialClone => lang_items.TrivialClone,
            SolverTraitLangItem::Field => lang_items.Field,
            SolverTraitLangItem::Freeze => lang_items.Freeze,
            SolverTraitLangItem::TryAsDyn => lang_items.TryAsDyn,
        };
        lang_item.expect("Lang item required but not found.").into()
    }

    fn require_adt_lang_item(self, lang_item: SolverAdtLangItem) -> AdtIdWrapper {
        let lang_items = self.lang_items();
        let lang_item = match lang_item {
            SolverAdtLangItem::Option => lang_items.Option.map(Into::into),
            SolverAdtLangItem::Poll => lang_items.Poll.map(Into::into),
            SolverAdtLangItem::DynMetadata => lang_items.DynMetadata.map(Into::into),
            SolverAdtLangItem::OwnedBox => lang_items.OwnedBox.map(Into::into),
        };
        AdtIdWrapper(lang_item.expect("Lang item required but not found."))
    }

    fn is_projection_lang_item(
        self,
        def_id: Self::TraitAssocTyId,
        lang_item: SolverProjectionLangItem,
    ) -> bool {
        self.as_projection_lang_item(def_id)
            .map_or(false, |l| std::mem::discriminant(&l) == std::mem::discriminant(&lang_item))
    }

    fn is_trait_lang_item(self, def_id: Self::TraitId, lang_item: SolverTraitLangItem) -> bool {
        is_lang_item!(
            SolverTraitLangItem, self, def_id.0, lang_item;

            Sized,
            MetaSized,
            PointeeSized,
            Unsize,
            Copy,
            Clone,
            DiscriminantKind,
            PointeeTrait,
            FnPtrTrait,
            Drop,
            Destruct,
            TransmuteTrait,
            Fn,
            FnMut,
            FnOnce,
            Future,
            Coroutine,
            Unpin,
            Tuple,
            Iterator,
            AsyncFn,
            AsyncFnMut,
            AsyncFnOnce,
            TrivialClone,
            AsyncFnKindHelper,
            AsyncIterator,
            BikeshedGuaranteedNoDrop,
            FusedIterator,
            Field,
            Freeze,
            TryAsDyn,
        )
    }

    fn as_projection_lang_item(
        self,
        def_id: Self::TraitAssocTyId,
    ) -> Option<SolverProjectionLangItem> {
        as_lang_item!(
            SolverProjectionLangItem, self, def_id.0, TypeAliasId;

            Metadata,
            CoroutineReturn,
            CoroutineYield,
            FutureOutput,
            CallRefFuture,
            CallOnceFuture,
            AsyncFnOnceOutput,
            AsyncFnKindUpvars,
            FieldBase,
            FieldType,
        )
    }

    fn as_trait_lang_item(self, def_id: Self::TraitId) -> Option<SolverTraitLangItem> {
        as_lang_item!(
            SolverTraitLangItem, self, def_id.0, TraitId;

            Sized,
            MetaSized,
            PointeeSized,
            Unsize,
            Copy,
            Clone,
            DiscriminantKind,
            PointeeTrait,
            FnPtrTrait,
            Drop,
            Destruct,
            TransmuteTrait,
            Fn,
            FnMut,
            FnOnce,
            Future,
            Coroutine,
            Unpin,
            Tuple,
            Iterator,
            AsyncFn,
            AsyncFnMut,
            AsyncFnOnce,
            TrivialClone,
            AsyncFnKindHelper,
            AsyncIterator,
            BikeshedGuaranteedNoDrop,
            FusedIterator,
            Field,
            Freeze,
            TryAsDyn,
        )
    }

    fn as_adt_lang_item(self, def_id: Self::AdtId) -> Option<SolverAdtLangItem> {
        as_lang_item!(
            SolverAdtLangItem, self, def_id.0, AdtId;

            Option,
            Poll,
            DynMetadata,
            OwnedBox,
        )
    }

    fn associated_type_def_ids(
        self,
        def_id: Self::TraitId,
    ) -> impl IntoIterator<Item = Self::DefId> {
        def_id.0.trait_items(self.db()).associated_types().map(|id| id.into())
    }

    fn for_each_relevant_impl<R: VisitorResult>(
        self,
        trait_ref: TraitRef<Self>,
        mut f: impl FnMut(Self::ImplId) -> R,
    ) -> R {
        let trait_def_id = trait_ref.def_id;
        let self_ty = trait_ref.self_ty();
        let krate = self.krate.expect("trait solving requires setting `DbInterner::krate`");
        let trait_block = trait_def_id.0.loc(self.db).container.block(self.db);
        let mut consider_impls_for_simplified_type = |simp: SimplifiedType<'_>| {
            let type_block = simp.def().and_then(|def_id| {
                let module = match def_id {
                    SolverDefId::AdtId(AdtId::StructId(id)) => id.module(self.db),
                    SolverDefId::AdtId(AdtId::EnumId(id)) => id.module(self.db),
                    SolverDefId::AdtId(AdtId::UnionId(id)) => id.module(self.db),
                    SolverDefId::TraitId(id) => id.module(self.db),
                    SolverDefId::TypeAliasId(id) => id.module(self.db),
                    SolverDefId::ConstId(_)
                    | SolverDefId::FunctionId(_)
                    | SolverDefId::ImplId(_)
                    | SolverDefId::BuiltinDeriveImplId(_)
                    | SolverDefId::StaticId(_)
                    | SolverDefId::InternedClosureId(_)
                    | SolverDefId::InternedCoroutineId(_)
                    | SolverDefId::InternedCoroutineClosureId(_)
                    | SolverDefId::InternedOpaqueTyId(_)
                    | SolverDefId::EnumVariantId(_)
                    | SolverDefId::AnonConstId(_)
                    | SolverDefId::Ctor(_) => return None,
                };
                module.block(self.db)
            });
            let in_block = type_block.is_some() || trait_block.is_some();
            let mut seen = Vec::new();
            let in_crates = TraitImpls::in_crate_and_deps_for_trait_and_self_ty(
                self.db,
                krate,
                trait_def_id.0,
                SelfTyKey(simp),
            );
            for &impl_ in in_crates {
                // The impls of a `const _` block are among those of its crate as well
                // as its own, and the solver takes a second copy for an ambiguity.
                if in_block {
                    seen.push(impl_);
                }
                try_visit!(f(impl_));
            }
            TraitImpls::for_each_block_trait_and_type(
                self.db,
                type_block,
                trait_block,
                &mut |impls| {
                    let (regular_impls, builtin_derive_impls) =
                        impls.for_trait_and_self_ty(trait_def_id.0, &simp);
                    for impl_ in regular_impls
                        .iter()
                        .map(|&it| AnyImplId::from(it))
                        .chain(builtin_derive_impls.iter().map(|&it| AnyImplId::from(it)))
                    {
                        if seen.contains(&impl_) {
                            continue;
                        }
                        seen.push(impl_);
                        try_visit!(f(impl_));
                    }
                    R::output()
                },
            )
        };

        match self_ty.kind() {
            TyKind::Bool
            | TyKind::Char
            | TyKind::Int(_)
            | TyKind::Uint(_)
            | TyKind::Float(_)
            | TyKind::Adt(_, _)
            | TyKind::Foreign(_)
            | TyKind::Str
            | TyKind::Array(_, _)
            | TyKind::Pat(_, _)
            | TyKind::Slice(_)
            | TyKind::RawPtr(_, _)
            | TyKind::Ref(_, _, _)
            | TyKind::FnDef(_, _)
            | TyKind::FnPtr(..)
            | TyKind::Dynamic(_, _)
            | TyKind::Closure(..)
            | TyKind::CoroutineClosure(..)
            | TyKind::Coroutine(_, _)
            | TyKind::Never
            | TyKind::Tuple(_)
            | TyKind::UnsafeBinder(_) => {
                let simp =
                    fast_reject::simplify_type(self, self_ty, fast_reject::TreatParams::AsRigid)
                        .unwrap();
                try_visit!(consider_impls_for_simplified_type(simp));
            }

            // HACK: For integer and float variables we have to manually look at all impls
            // which have some integer or float as a self type.
            TyKind::Infer(InferTy::IntVar(_)) => {
                use IntTy::*;
                use UintTy::*;
                // This causes a compiler error if any new integer kinds are added.
                let (I8 | I16 | I32 | I64 | I128 | Isize): IntTy;
                let (U8 | U16 | U32 | U64 | U128 | Usize): UintTy;
                let possible_integers = [
                    // signed integers
                    SimplifiedType::Int(I8),
                    SimplifiedType::Int(I16),
                    SimplifiedType::Int(I32),
                    SimplifiedType::Int(I64),
                    SimplifiedType::Int(I128),
                    SimplifiedType::Int(Isize),
                    // unsigned integers
                    SimplifiedType::Uint(U8),
                    SimplifiedType::Uint(U16),
                    SimplifiedType::Uint(U32),
                    SimplifiedType::Uint(U64),
                    SimplifiedType::Uint(U128),
                    SimplifiedType::Uint(Usize),
                ];
                for simp in possible_integers {
                    try_visit!(consider_impls_for_simplified_type(simp));
                }
            }

            TyKind::Infer(InferTy::FloatVar(_)) => {
                // This causes a compiler error if any new float kinds are added.
                let (FloatTy::F16 | FloatTy::F32 | FloatTy::F64 | FloatTy::F128);
                let possible_floats = [
                    SimplifiedType::Float(FloatTy::F16),
                    SimplifiedType::Float(FloatTy::F32),
                    SimplifiedType::Float(FloatTy::F64),
                    SimplifiedType::Float(FloatTy::F128),
                ];

                for simp in possible_floats {
                    try_visit!(consider_impls_for_simplified_type(simp));
                }
            }

            // The only traits applying to aliases and placeholders are blanket impls.
            //
            // Impls which apply to an alias after normalization are handled by
            // `assemble_candidates_after_normalizing_self_ty`.
            TyKind::Alias(..) | TyKind::Placeholder(..) | TyKind::Error(_) => (),

            // FIXME: These should ideally not exist as a self type. It would be nice for
            // the builtin auto trait impls of coroutines to instead directly recurse
            // into the witness.
            TyKind::CoroutineWitness(..) => (),

            // These variants should not exist as a self type.
            TyKind::Infer(
                InferTy::TyVar(_)
                | InferTy::FreshTy(_)
                | InferTy::FreshIntTy(_)
                | InferTy::FreshFloatTy(_),
            )
            | TyKind::Param(_)
            | TyKind::Bound(_, _) => panic!("unexpected self type: {self_ty:?}"),
        }

        self.for_each_blanket_impl(trait_def_id, f)
    }

    fn for_each_blanket_impl<R: VisitorResult>(
        self,
        trait_def_id: Self::TraitId,
        mut f: impl FnMut(Self::ImplId) -> R,
    ) -> R {
        let Some(krate) = self.krate else {
            return R::output();
        };
        let block = trait_def_id.0.loc(self.db).container.block(self.db);

        // The impls of a block are among those of its crate as well as its own, and the solver
        // takes a second copy for an ambiguity.
        let mut seen = Vec::new();
        try_visit!(TraitImpls::for_each_block(self.db, block, &mut |impls| {
            for &impl_ in impls.blanket_impls(trait_def_id.0) {
                seen.push(impl_);
                try_visit!(f(impl_.into()));
            }
            R::output()
        }));
        for &impl_ in TraitImpls::blanket_in_crate_and_deps(self.db, krate, trait_def_id.0) {
            if !seen.contains(&impl_) {
                try_visit!(f(impl_.into()));
            }
        }
        R::output()
    }

    fn has_item_definition(self, _def_id: Self::ImplOrTraitAssocTermId) -> bool {
        // FIXME(next-solver): should check if the associated item has a value.
        true
    }

    fn impl_is_default(self, impl_def_id: Self::ImplId) -> bool {
        match impl_def_id {
            AnyImplId::ImplId(impl_id) => ImplSignature::of(self.db, impl_id).is_default(),
            AnyImplId::BuiltinDeriveImplId(_) => false,
        }
    }

    #[tracing::instrument(skip(self), ret)]
    fn impl_trait_ref(
        self,
        impl_id: Self::ImplId,
    ) -> EarlyBinder<Self, rustc_type_ir::TraitRef<Self>> {
        match impl_id {
            AnyImplId::ImplId(impl_id) => {
                let db = self.db();
                db.impl_trait(impl_id)
                    // ImplIds for impls where the trait ref can't be resolved should never reach trait solving
                    .expect("invalid impl passed to trait solver")
            }
            AnyImplId::BuiltinDeriveImplId(impl_id) => {
                crate::builtin_derive::impl_trait(self, impl_id)
            }
        }
    }

    fn impl_polarity(self, impl_id: Self::ImplId) -> rustc_type_ir::ImplPolarity {
        let AnyImplId::ImplId(impl_id) = impl_id else {
            return ImplPolarity::Positive;
        };
        let impl_data = ImplSignature::of(self.db(), impl_id);
        if impl_data.flags.contains(ImplFlags::NEGATIVE) {
            ImplPolarity::Negative
        } else {
            ImplPolarity::Positive
        }
    }

    fn trait_is_auto(self, trait_: Self::TraitId) -> bool {
        let trait_data = TraitSignature::of(self.db(), trait_.0);
        trait_data.flags.contains(TraitFlags::AUTO)
    }

    fn trait_is_alias(self, trait_: Self::TraitId) -> bool {
        let trait_data = TraitSignature::of(self.db(), trait_.0);
        trait_data.flags.contains(TraitFlags::ALIAS)
    }

    fn trait_is_marker(self, trait_: Self::TraitId) -> bool {
        AttrFlags::has_word(self.db, trait_.0.into(), "marker")
    }

    fn supertrait_def_ids(self, trait_: Self::TraitId) -> impl Iterator<Item = Self::TraitId> {
        crate::utils::all_super_traits(self.db, trait_.0).iter().map(|&it| TraitIdWrapper(it))
    }

    fn is_fully_generic_for_reflection(self, impl_id: Self::ImplId) -> bool {
        fully_generic_for_reflection(
            self.impl_trait_ref(impl_id).skip_binder().args,
            self.own_clauses_of(impl_id.into()).skip_binder(),
        )
    }

    fn trait_is_dyn_compatible(self, trait_: Self::TraitId) -> bool {
        crate::dyn_compatibility::dyn_compatibility(self.db(), trait_.0).is_none()
    }

    fn trait_is_fundamental(self, trait_: Self::TraitId) -> bool {
        let trait_data = TraitSignature::of(self.db(), trait_.0);
        trait_data.flags.contains(TraitFlags::FUNDAMENTAL)
    }

    fn delay_bug(self, _msg: impl ToString) -> Self::ErrorGuaranteed {
        ErrorGuaranteed
    }

    fn span_delayed_bug(self, _span: Self::Span, _msg: impl ToString) -> Self::ErrorGuaranteed {
        ErrorGuaranteed
    }

    fn is_general_coroutine(self, def_id: Self::CoroutineId) -> bool {
        matches!(def_id.0.loc(self.db).kind, HirClosureKind::OldCoroutine(_))
    }

    fn coroutine_is_async(self, def_id: Self::CoroutineId) -> bool {
        matches!(
            def_id.0.loc(self.db).kind,
            HirClosureKind::Coroutine { kind: HirCoroutineKind::Async, .. }
        )
    }

    fn coroutine_is_gen(self, def_id: Self::CoroutineId) -> bool {
        matches!(
            def_id.0.loc(self.db).kind,
            HirClosureKind::Coroutine { kind: HirCoroutineKind::Gen, .. }
        )
    }

    fn coroutine_is_async_gen(self, def_id: Self::CoroutineId) -> bool {
        matches!(
            def_id.0.loc(self.db).kind,
            HirClosureKind::Coroutine { kind: HirCoroutineKind::AsyncGen, .. }
        )
    }

    fn unsizing_params_for_adt(self, id: Self::AdtId) -> Self::UnsizingParams {
        let def = AdtDef::new(id.0, self);
        let num_params = self.generics_of(id.into()).count();

        let maybe_unsizing_param_idx = |arg: GenericArg<'db>| match arg.kind() {
            GenericArgKind::Type(ty) => match ty.kind() {
                rustc_type_ir::TyKind::Param(p) => Some(p.index),
                _ => None,
            },
            GenericArgKind::Lifetime(_) => None,
            GenericArgKind::Const(ct) => match ct.kind() {
                rustc_type_ir::ConstKind::Param(p) => Some(p.index),
                _ => None,
            },
        };

        // The last field of the structure has to exist and contain type/const parameters.
        let variant = match def.def_id() {
            AdtId::StructId(id) => VariantId::from(id),
            AdtId::UnionId(id) => id.into(),
            AdtId::EnumId(_) => panic!("expected a struct or a union"),
        };
        let fields = variant.fields(self.db());
        let mut prefix_fields = fields.fields().iter();
        let Some(tail_field) = prefix_fields.next_back() else {
            return UnsizingParams(DenseBitSet::new_empty(num_params));
        };

        let field_types = self.db().field_types(variant);
        let mut unsizing_params = DenseBitSet::new_empty(num_params);
        let ty = field_types[tail_field.0].ty();
        for arg in ty.instantiate_identity().skip_norm_wip().walk() {
            if let Some(i) = maybe_unsizing_param_idx(arg) {
                unsizing_params.insert(i);
            }
        }

        // Ensure none of the other fields mention the parameters used
        // in unsizing.
        for field in prefix_fields {
            for arg in field_types[field.0].ty().instantiate_identity().skip_norm_wip().walk() {
                if let Some(i) = maybe_unsizing_param_idx(arg) {
                    unsizing_params.remove(i);
                }
            }
        }

        UnsizingParams(unsizing_params)
    }

    fn anonymize_bound_vars<T: rustc_type_ir::TypeFoldable<Self>>(
        self,
        value: rustc_type_ir::Binder<Self, T>,
    ) -> rustc_type_ir::Binder<Self, T> {
        struct Anonymize<'a, 'db> {
            interner: DbInterner<'db>,
            map: &'a mut FxIndexMap<BoundVar, BoundVariableKind<'db>>,
        }
        impl<'db> BoundVarReplacerDelegate<DbInterner<'db>> for Anonymize<'_, 'db> {
            fn replace_region(&mut self, br: BoundRegion<'db>) -> Region<'db> {
                let entry = self.map.entry(br.var);
                let index = entry.index();
                let var = BoundVar::from_usize(index);
                let kind = (*entry
                    .or_insert_with(|| BoundVariableKind::Region(BoundRegionKind::Anon)))
                .expect_region();
                let br = BoundRegion { var, kind };
                Region::new_bound(self.interner, DebruijnIndex::ZERO, br)
            }
            fn replace_ty(&mut self, bt: BoundTy<'db>) -> Ty<'db> {
                let entry = self.map.entry(bt.var);
                let index = entry.index();
                let var = BoundVar::from_usize(index);
                let kind = (*entry.or_insert_with(|| BoundVariableKind::Ty(BoundTyKind::Anon)))
                    .expect_ty();
                Ty::new_bound(self.interner, DebruijnIndex::ZERO, BoundTy { var, kind })
            }
            fn replace_const(&mut self, bv: BoundConst<'db>) -> Const<'db> {
                let entry = self.map.entry(bv.var);
                let index = entry.index();
                let var = BoundVar::from_usize(index);
                let () = (*entry.or_insert_with(|| BoundVariableKind::Const)).expect_const();
                Const::new_bound(self.interner, DebruijnIndex::ZERO, BoundConst::new(var))
            }
        }

        let mut map = Default::default();
        let delegate = Anonymize { interner: self, map: &mut map };
        let inner = self.replace_escaping_bound_vars_uncached(value.skip_binder(), delegate);
        let bound_vars = BoundVarKinds::new_from_iter(map.into_values());
        Binder::bind_with_vars(inner, bound_vars)
    }

    fn opaque_types_defined_by(self, def_id: Self::LocalDefId) -> Self::LocalDefIds {
        let Ok(def_id) = InferBodyId::try_from(def_id) else {
            return SolverDefIds::default();
        };
        let mut result = Vec::new();
        crate::opaques::opaque_types_defined_by(self.db, def_id, &mut result);
        SolverDefIds::new_from_slice(&result)
    }

    fn opaque_types_and_coroutines_defined_by(self, def_id: Self::LocalDefId) -> Self::LocalDefIds {
        let db = self.db;

        let Ok(def_id) = InferBodyId::try_from(def_id) else {
            return SolverDefIds::default();
        };
        let mut result = Vec::new();

        crate::opaques::opaque_types_defined_by(db, def_id, &mut result);

        // Collect coroutines.
        let (store, root_expr) = def_id.store_and_root_expr(db);
        // We can't just visit all exprs, since this may end up in unrelated anon consts.
        CoroutinesVisitor { db: self.db, owner: def_id, store, coroutines: &mut result }
            .on_expr(root_expr);

        return SolverDefIds::new_from_slice(&result);

        struct CoroutinesVisitor<'a, 'db> {
            db: &'db dyn HirDatabase,
            owner: InferBodyId<'db>,
            store: &'db ExpressionStore,
            coroutines: &'a mut Vec<SolverDefId<'db>>,
        }

        impl<'db> StoreVisitor for CoroutinesVisitor<'_, 'db> {
            fn on_expr(&mut self, expr: ExprId) {
                if let hir_def::hir::Expr::Closure {
                    closure_kind:
                        kind @ (hir_def::hir::ClosureKind::Coroutine { .. }
                        | hir_def::hir::ClosureKind::OldCoroutine(_)),
                    ..
                } = self.store.expr(expr)
                {
                    let coroutine = InternedCoroutineId::new(
                        self.db,
                        InternedClosure { owner: self.owner, expr, kind },
                    );
                    self.coroutines.push(coroutine.into());
                }

                self.store.visit_expr_children(expr, self);
            }
            fn on_pat(&mut self, pat: PatId) {
                self.store.visit_pat_children(pat, self);
            }
            // Do not visit anon consts, they're separate bodies.
            fn on_anon_const_expr(&mut self, _expr: ExprId) {}
        }
    }

    fn alias_has_const_conditions(self, _def_id: Self::DefId) -> bool {
        // FIXME(next-solver)
        false
    }

    fn explicit_implied_const_bounds(
        self,
        def_id: Self::DefId,
    ) -> EarlyBinder<
        Self,
        impl IntoIterator<Item = rustc_type_ir::Binder<Self, rustc_type_ir::TraitRef<Self>>>,
    > {
        // A trait's `[const]` supertraits. FIXME: an associated type's `[const]` item bounds.
        let SolverDefId::TraitId(trait_) = def_id else {
            return EarlyBinder::bind(self, Vec::new());
        };
        let conditions = crate::lower::const_conditions(self.db(), trait_.into()).skip_binder();
        EarlyBinder::bind(
            self,
            crate::effects::effect_trait_refs(conditions.iter().copied())
                .filter(|it| {
                    it.skip_binder().def_id.0 != trait_ && is_ty_self(it.skip_binder().self_ty())
                })
                .collect(),
        )
    }

    fn fn_is_const(self, id: Self::FunctionId) -> bool {
        match id.0 {
            CallableDefId::FunctionId(id) => crate::effects::fn_is_const(self.db(), id),
            CallableDefId::StructId(_) | CallableDefId::EnumVariantId(_) => true,
        }
    }

    fn impl_is_const(self, def_id: Self::ImplId) -> bool {
        crate::effects::impl_is_const(self.db(), def_id)
    }

    fn opt_alias_variances(
        self,
        _kind: impl Into<AliasTermKind<'db>>,
    ) -> Option<Self::VariancesOf> {
        None
    }

    fn opaque_of_impl(
        self,
        param_env: Self::ParamEnv,
        def_id: Self::OpaqueTyId,
        args: Self::GenericArgs,
    ) -> Option<Self::Ty> {
        let env = crate::ParamEnvAndCrate { param_env, krate: self.krate? };
        self.db.opaque_of_impl(env, def_id.0, args)
    }

    fn type_of_opaque_hir_typeck(
        self,
        opaque: Self::LocalOpaqueTyId,
    ) -> EarlyBinder<Self, Self::Ty> {
        let impl_trait_id = opaque.0.loc(self.db);
        // The entry is missing when this call cycles back into the still-running inference
        // of the defining body, as the cycle fallback is an empty result.
        let hidden_type = match impl_trait_id {
            crate::ImplTraitId::ReturnTypeImplTrait(func, idx) => {
                crate::opaques::rpit_hidden_types(self.db, func).get(idx)
            }
            crate::ImplTraitId::TypeAliasImplTrait(type_alias, idx) => {
                crate::opaques::tait_hidden_types(self.db, type_alias).get(idx)
            }
        };
        match hidden_type {
            Some(hidden_type) => hidden_type.get(),
            None => EarlyBinder::bind(self, Ty::new_error(self, ErrorGuaranteed)),
        }
    }

    fn coroutine_hidden_types(
        self,
        _def_id: Self::CoroutineId,
    ) -> EarlyBinder<Self, Binder<'db, CoroutineWitnessTypes<Self>>> {
        // FIXME: Actually implement this.
        EarlyBinder::bind(
            self,
            Binder::dummy(CoroutineWitnessTypes {
                types: Tys::default(),
                assumptions: RegionAssumptions::default(),
            }),
        )
    }

    fn is_default_trait(self, def_id: Self::TraitId) -> bool {
        self.as_trait_lang_item(def_id).map_or(false, |l| matches!(l, SolverTraitLangItem::Sized))
    }

    fn trait_is_coinductive(self, trait_: Self::TraitId) -> bool {
        TraitSignature::of(self.db(), trait_.0).flags.contains(TraitFlags::COINDUCTIVE)
    }

    fn trait_is_unsafe(self, trait_: Self::TraitId) -> bool {
        TraitSignature::of(self.db(), trait_.0).flags.contains(TraitFlags::UNSAFE)
    }

    fn impl_self_is_guaranteed_unsized(self, _def_id: Self::ImplId) -> bool {
        false
    }

    fn impl_specializes(
        self,
        specializing_impl_def_id: Self::ImplId,
        parent_impl_def_id: Self::ImplId,
    ) -> bool {
        let (AnyImplId::ImplId(specializing_impl_def_id), AnyImplId::ImplId(parent_impl_def_id)) =
            (specializing_impl_def_id, parent_impl_def_id)
        else {
            // No builtin derive allow specialization currently.
            return false;
        };
        crate::specialization::specializes(self.db, specializing_impl_def_id, parent_impl_def_id)
    }

    fn next_trait_solver_globally(self) -> bool {
        true
    }

    type Probe = rustc_type_ir::solve::inspect::Probe<DbInterner<'db>>;
    fn mk_probe(self, probe: rustc_type_ir::solve::inspect::Probe<Self>) -> Self::Probe {
        probe
    }
    fn evaluate_root_goal_for_proof_tree_raw(
        self,
        canonical_goal: Self::CanonicalInput,
        root_depth: usize,
    ) -> (QueryResult<Self>, Self::Probe, RequiredDepth) {
        rustc_next_trait_solver::solve::evaluate_root_goal_for_proof_tree_raw_provider::<
            SolverContext<'db>,
            Self,
        >(self, canonical_goal, root_depth)
    }

    fn is_sizedness_trait(self, def_id: Self::TraitId) -> bool {
        matches!(
            self.as_trait_lang_item(def_id),
            Some(SolverTraitLangItem::Sized | SolverTraitLangItem::MetaSized)
        )
    }

    fn closure_is_const(self, def_id: Self::ClosureId) -> bool {
        crate::effects::closure_is_const(self.db(), def_id.0)
    }

    /// The item's name; `'_` for an item without one (an impl, closure, coroutine, opaque or
    /// anonymous const), which `BoundRegionKind::get_name` reads as no name.
    fn item_name(self, item: Self::DefId) -> Self::Symbol {
        use hir_def::signatures::{
            ConstSignature, FunctionSignature, StaticSignature, TypeAliasSignature,
        };

        let db = self.db;
        let name = match item {
            SolverDefId::AdtId(AdtId::StructId(id)) | SolverDefId::Ctor(Ctor::Struct(id)) => {
                Some(*StructSignature::of(db, id).name.symbol())
            }
            SolverDefId::AdtId(AdtId::EnumId(id)) => Some(*EnumSignature::of(db, id).name.symbol()),
            SolverDefId::AdtId(AdtId::UnionId(id)) => {
                Some(*UnionSignature::of(db, id).name.symbol())
            }
            SolverDefId::ConstId(id) => {
                ConstSignature::of(db, id).name.as_ref().map(|it| *it.symbol())
            }
            SolverDefId::FunctionId(id) => Some(*FunctionSignature::of(db, id).name.symbol()),
            SolverDefId::StaticId(id) => Some(*StaticSignature::of(db, id).name.symbol()),
            SolverDefId::TraitId(id) => Some(*TraitSignature::of(db, id).name.symbol()),
            SolverDefId::TypeAliasId(id) => Some(*TypeAliasSignature::of(db, id).name.symbol()),
            SolverDefId::EnumVariantId(id) | SolverDefId::Ctor(Ctor::Enum(id)) => {
                id.loc(db).parent.enum_variants(db).variant_name_by_id(id).map(|it| *it.symbol())
            }
            SolverDefId::ImplId(_)
            | SolverDefId::BuiltinDeriveImplId(_)
            | SolverDefId::AnonConstId(_)
            | SolverDefId::InternedClosureId(_)
            | SolverDefId::InternedCoroutineId(_)
            | SolverDefId::InternedCoroutineClosureId(_)
            | SolverDefId::InternedOpaqueTyId(_) => None,
        };
        Symbol(name.unwrap_or(sym::tick_underscore))
    }

    #[inline]
    fn get_re_static_lifetime(self) -> rustc_type_ir::Region<Self> {
        self.default_types().regions.statik
    }

    #[inline]
    fn intern_re_var(self, vid: RegionVid) -> rustc_type_ir::Region<Self> {
        self.intern_region(RegionKind::ReVar(vid))
    }

    #[inline]
    fn intern_region(self, kind: RegionKind<Self>) -> rustc_type_ir::Region<Self> {
        rustc_type_ir::Region(InternedRegion::intern(kind))
    }

    #[inline]
    fn intern_bound_region(
        self,
        debruijn: DebruijnIndex,
        bound_region: BoundRegion<'db>,
    ) -> rustc_type_ir::Region<Self> {
        self.intern_region(RegionKind::ReBound(BoundVarIndexKind::Bound(debruijn), bound_region))
    }

    #[inline]
    fn intern_canonical_bound(self, var: BoundVar) -> rustc_type_ir::Region<Self> {
        self.intern_region(RegionKind::ReBound(
            BoundVarIndexKind::Canonical,
            BoundRegion { var, kind: BoundRegionKind::Anon },
        ))
    }

    #[inline]
    fn intern_valtree(self, kind: ValTreeKind<Self>) -> Self::ValTree {
        ValTree::new(kind)
    }

    type CanonicalInput = CanonicalInput<'db>;

    #[inline]
    fn mk_canonical_input(self, data: CanonicalInputData<'db>) -> Self::CanonicalInput {
        CanonicalInput::new(data)
    }

    /// rustc's `ValTree::zst`: an aggregate of no fields.
    #[inline]
    fn const_zst(self) -> Self::ValTree {
        ValTree::zst()
    }
}

fn is_ty_self(ty: Ty<'_>) -> bool {
    match ty.kind() {
        TyKind::Param(param) => param.index == 0,
        _ => false,
    }
}
fn is_clause_at_ty(p: &Clause<'_>, filter: impl FnOnce(Ty<'_>) -> bool) -> bool {
    match p.kind().skip_binder() {
        // rustc has the following assertion:
        // https://github.com/rust-lang/rust/blob/52618eb338609df44978b0ca4451ab7941fd1c7a/compiler/rustc_hir_analysis/src/hir_ty_lowering/bounds.rs#L525-L608
        ClauseKind::Trait(it) => filter(it.self_ty()),
        ClauseKind::TypeOutlives(it) => filter(it.0),
        ClauseKind::Projection(it) => filter(it.self_ty()),
        ClauseKind::HostEffect(it) => filter(it.self_ty()),
        _ => false,
    }
}

/// rustc's rule for `TypingMode::Reflection`, which ignores lifetimes: an impl whose header has
/// `args` is generic over every lifetime the header names, each a parameter of its own, and
/// its where clauses constrain none of them (name no free region, `'static` included).
fn fully_generic_for_reflection<'db>(
    args: GenericArgs<'db>,
    clauses: impl IntoIterator<Item = Clause<'db>>,
) -> bool {
    let mut params = Vec::new();
    let header_generic = args.iter().all(|arg| {
        arg.walk().all(|arg| match arg.kind() {
            GenericArgKind::Lifetime(region) => match region.kind() {
                RegionKind::ReEarlyParam(param) if !params.contains(&param.index) => {
                    params.push(param.index);
                    true
                }
                RegionKind::ReBound(..) => true,
                _ => false,
            },
            GenericArgKind::Type(_) | GenericArgKind::Const(_) => true,
        })
    });
    header_generic && clauses.into_iter().all(|clause| !clause.has_free_regions())
}

/// Runs `f` on this thread's [`PARAM_ENV_CACHE`], taken out while `f` runs, so a call within
/// it gets a cache of its own.
fn with_param_env_cache<'db, R>(f: impl FnOnce(&mut ParamEnvCache<'db>) -> R) -> R {
    let mut cache = PARAM_ENV_CACHE.take().unwrap_or_default();
    if cache.0.len() >= PARAM_ENV_CACHE_ENTRIES {
        cache.0.clear();
    }
    let result = f(table::unerase_mut::<ParamEnvCache<'db>>(&mut cache));
    PARAM_ENV_CACHE.set(Some(cache));
    result
}

/// The item an alias names.
fn alias_term_def_id(kind: AliasTermKind<'_>) -> SolverDefId<'_> {
    match kind {
        AliasTermKind::ProjectionTy { def_id } => def_id.into(),
        AliasTermKind::InherentTy { def_id } => def_id.into(),
        AliasTermKind::OpaqueTy { def_id } => def_id.into(),
        AliasTermKind::FreeTy { def_id } => def_id.into(),
        AliasTermKind::AnonConst { def_id } => def_id.into(),
        AliasTermKind::ProjectionConst { def_id } => def_id.into(),
        AliasTermKind::FreeConst { def_id } => def_id.into(),
        AliasTermKind::InherentConstSelf { def_id }
        | AliasTermKind::InherentConstImpl { def_id } => def_id.into(),
    }
}

/// Whether `args` are one of each of `params`, in order and of its kind.
fn args_fit_params(
    params: impl IntoIterator<Item = GenericParamId>,
    args: &[GenericArg<'_>],
) -> bool {
    let mut args = args.iter();
    params.into_iter().all(|param| {
        args.next().is_some_and(|arg| {
            matches!(
                (param, arg.kind()),
                (GenericParamId::LifetimeParamId(_), GenericArgKind::Lifetime(_))
                    | (GenericParamId::TypeParamId(_), GenericArgKind::Type(_))
                    | (GenericParamId::ConstParamId(_), GenericArgKind::Const(_))
            )
        })
    }) && args.next().is_none()
}

impl<'db> DbInterner<'db> {
    pub fn shift_bound_var_indices<T>(self, bound_vars: usize, value: T) -> T
    where
        T: rustc_type_ir::TypeFoldable<Self>,
    {
        let shift_bv = |bv: BoundVar| BoundVar::from_usize(bv.as_usize() + bound_vars);
        self.replace_escaping_bound_vars_uncached(
            value,
            FnMutDelegate {
                regions: &mut |r: BoundRegion<'db>| {
                    Region::new_bound(
                        self,
                        DebruijnIndex::ZERO,
                        BoundRegion { var: shift_bv(r.var), kind: r.kind },
                    )
                },
                types: &mut |t: BoundTy<'db>| {
                    Ty::new_bound(
                        self,
                        DebruijnIndex::ZERO,
                        BoundTy { var: shift_bv(t.var), kind: t.kind },
                    )
                },
                consts: &mut |c| {
                    Const::new_bound(self, DebruijnIndex::ZERO, BoundConst::new(shift_bv(c.var)))
                },
            },
        )
    }

    pub fn replace_escaping_bound_vars_uncached<T: rustc_type_ir::TypeFoldable<DbInterner<'db>>>(
        self,
        value: T,
        delegate: impl BoundVarReplacerDelegate<DbInterner<'db>>,
    ) -> T {
        if !value.has_escaping_bound_vars() {
            value
        } else {
            let mut replacer = BoundVarReplacer::new(self, delegate);
            value.fold_with(&mut replacer)
        }
    }

    pub fn replace_bound_vars_uncached<T: rustc_type_ir::TypeFoldable<DbInterner<'db>>>(
        self,
        value: Binder<'db, T>,
        delegate: impl BoundVarReplacerDelegate<DbInterner<'db>>,
    ) -> T {
        self.replace_escaping_bound_vars_uncached(value.skip_binder(), delegate)
    }

    // FIXME: add splat support when the experiment is complete
    pub fn mk_fn_sig<I>(
        self,
        inputs: I,
        output: Ty<'db>,
        c_variadic: bool,
        safety: Safety,
        abi: ExternAbi,
    ) -> FnSig<'db>
    where
        I: IntoIterator<Item = Ty<'db>>,
    {
        FnSig {
            inputs_and_output: Tys::new_from_iter(
                inputs.into_iter().chain(std::iter::once(output)),
            ),
            fn_sig_kind: FnSigKind::default()
                .set_abi(abi)
                .set_safety(safety)
                .set_c_variadic(c_variadic),
        }
    }

    /// `mk_fn_sig`, but with a safe Rust ABI, and no C-variadic argument.
    pub fn mk_fn_sig_safe_rust_abi<I>(self, inputs: I, output: Ty<'db>) -> FnSig<'db>
    where
        I: IntoIterator<Item = Ty<'db>>,
    {
        self.mk_fn_sig(inputs, output, false, Safety::Safe, ExternAbi::Rust)
    }
}

fn generic_predicates_of<'db>(
    db: &'db dyn HirDatabase,
    def_id: SolverDefId<'db>,
) -> &'db GenericPredicates {
    match def_id {
        SolverDefId::BuiltinDeriveImplId(impl_) => crate::builtin_derive::predicates(db, impl_),
        SolverDefId::AnonConstId(anon_const) => {
            let loc = anon_const.loc(db);
            if loc.allow_using_generic_params {
                GenericPredicates::query(db, loc.owner.generic_def(db))
            } else {
                GenericPredicates::empty()
            }
        }
        _ => GenericPredicates::query(db, def_id.try_into().unwrap()),
    }
}

macro_rules! TrivialTypeTraversalImpls {
    ($($ty:ty,)+) => {
        $(
            impl<'db> rustc_type_ir::TypeFoldable<DbInterner<'db>> for $ty {
                fn try_fold_with<F: rustc_type_ir::FallibleTypeFolder<DbInterner<'db>>>(
                    self,
                    _: &mut F,
                ) -> ::std::result::Result<Self, F::Error> {
                    Ok(self)
                }

                #[inline]
                fn fold_with<F: rustc_type_ir::TypeFolder<DbInterner<'db>>>(
                    self,
                    _: &mut F,
                ) -> Self {
                    self
                }
            }

            impl<'db> rustc_type_ir::TypeVisitable<DbInterner<'db>> for $ty {
                #[inline]
                fn visit_with<F: rustc_type_ir::TypeVisitor<DbInterner<'db>>>(
                    &self,
                    _: &mut F)
                    -> F::Result
                {
                    <F::Result as rustc_ast_ir::visit::VisitorResult>::output()
                }
            }

            // SAFETY: every type listed holds ids and plain data, no interned type, so there
            // is nothing to visit.
            unsafe impl<V> rustc_type_ir::GenericTypeVisitable<V> for $ty {
                #[inline]
                fn generic_visit_with(&self, _visitor: &mut V) {}
            }
        )+
    };
}

TrivialTypeTraversalImpls! {
    SolverDefId<'_>,
    TraitIdWrapper,
    TypeAliasIdWrapper,
    CallableIdWrapper,
    ClosureIdWrapper<'_>,
    CoroutineIdWrapper<'_>,
    CoroutineClosureIdWrapper<'_>,
    AdtIdWrapper,
    TraitAssocTyId,
    TraitAssocConstId,
    TraitAssocTermId,
    ImplOrTraitAssocTyId,
    ImplOrTraitAssocConstId,
    ImplOrTraitAssocTermId,
    FreeTyAliasId,
    FreeConstAliasId,
    FreeTermAliasId,
    InherentAssocTyId,
    InherentAssocConstId,
    InherentAssocTermId,
    OpaqueTyIdWrapper<'_>,
    AnyImplId,
    AnonConstIdWrapper<'_>,
    Safety,
    Span,
    ParamConst,
    ParamTy,
    EarlyParamRegion,
    AdtDef,
    ScalarInt,
}

mod tls_db {
    use std::{cell::Cell, ptr::NonNull};

    use crate::db::HirDatabase;

    struct Attached {
        database: Cell<Option<NonNull<dyn HirDatabase>>>,
    }

    impl Attached {
        #[inline]
        fn attach<R>(&self, db: &dyn HirDatabase, op: impl FnOnce() -> R) -> R {
            struct DbGuard<'s> {
                state: Option<&'s Attached>,
            }

            impl<'s> DbGuard<'s> {
                #[inline]
                fn new(attached: &'s Attached, db: &dyn HirDatabase) -> Self {
                    match attached.database.get() {
                        Some(current_db) => {
                            let new_db = NonNull::from(db);
                            if !std::ptr::addr_eq(current_db.as_ptr(), new_db.as_ptr()) {
                                panic!(
                                    "Cannot change attached database. This is likely a bug.\n\
                                    If this is not a bug, you can use `attach_db_allow_change()`."
                                );
                            }
                            Self { state: None }
                        }
                        None => {
                            // Otherwise, set the database.
                            attached.database.set(Some(NonNull::from(db)));
                            Self { state: Some(attached) }
                        }
                    }
                }
            }

            impl Drop for DbGuard<'_> {
                #[inline]
                fn drop(&mut self) {
                    // Reset database to null if we did anything in `DbGuard::new`.
                    if let Some(attached) = self.state {
                        attached.database.set(None);
                    }
                }
            }

            let _guard = DbGuard::new(self, db);
            op()
        }

        #[inline]
        fn attach_allow_change<R>(&self, db: &dyn HirDatabase, op: impl FnOnce() -> R) -> R {
            struct DbGuard<'s> {
                state: &'s Attached,
                prev: Option<NonNull<dyn HirDatabase>>,
            }

            impl<'s> DbGuard<'s> {
                #[inline]
                fn new(attached: &'s Attached, db: &dyn HirDatabase) -> Self {
                    let prev = attached.database.replace(Some(NonNull::from(db)));
                    Self { state: attached, prev }
                }
            }

            impl Drop for DbGuard<'_> {
                #[inline]
                fn drop(&mut self) {
                    self.state.database.set(self.prev);
                }
            }

            let _guard = DbGuard::new(self, db);
            op()
        }

        #[inline]
        fn with<R>(&self, op: impl FnOnce(&dyn HirDatabase) -> R) -> R {
            let db = self.database.get().expect("Tried to use attached db, but no db is attached");

            // SAFETY: `database` holds a pointer only while the `DbGuard` that set it is alive,
            // which borrows the database for that long and puts back the pointer before it on
            // drop, unwinding too; `op` runs within, and the reference it gets, of a lifetime
            // of its own, cannot leave it.
            op(unsafe { db.as_ref() })
        }
    }

    thread_local! {
        static GLOBAL_DB: Attached = const { Attached { database: Cell::new(None) } };
    }

    #[inline]
    pub fn attach_db<R>(db: &dyn HirDatabase, op: impl FnOnce() -> R) -> R {
        GLOBAL_DB.with(|global_db| global_db.attach(db, op))
    }

    #[inline]
    pub fn attach_db_allow_change<R>(db: &dyn HirDatabase, op: impl FnOnce() -> R) -> R {
        GLOBAL_DB.with(|global_db| global_db.attach_allow_change(db, op))
    }

    #[inline]
    pub fn with_attached_db<R>(op: impl FnOnce(&dyn HirDatabase) -> R) -> R {
        GLOBAL_DB.with(
            #[inline]
            |a| a.with(op),
        )
    }
}

mod tls_cache;

/// The tables every interned kind is an index into (`docs/v2/plan/design/data.md`, "types"). A
/// value is written once, at a row that never moves and is never freed, so its index names it
/// for the life of the process, on every thread, in every revision. A value is found by its
/// hash in an [`Index`]: no lock on a hit, the lock of one shard of the index to add a value.
///
/// - [`Rows`], for the kinds with a structure (types, regions, constants, predicates,
///   patterns): the flags and outer exclusive binder in hot columns, a `u8` tag column, and the
///   kind whole (its children `u32` indices, so it is 4 to 32 bytes) in a cold one.
/// - [`Lists`], for the lists (generic arguments, types, clauses, ...): a start and a length
///   column over one column of items (CSR), and for clauses the flags of the whole list.
/// - [`Values`], for the kinds whose API hands out references (value trees, allocations, the
///   solver's external constraints).
///
/// This module is the next solver's unsafe code (`docs/v2/plan/design/unsafe.md`), behind a
/// safe API: no row number, however made, reads memory not written. A read checks its row was
/// published (one acquire load of the row's tag, length or mark, which the table's own lookups
/// skip, as the index names only published rows), and a kind goes in and out under its
/// `'static` name through [`Erase`]. What unsafe buys here, against the safe version of each:
///
    /// - a column of `OnceLock<T>` segments: a state word per value (a type row's 4-byte flags
    ///   and binder take 8 each) and a state check on every read of every column;
/// - an index whose grown tables are reference counted rather than leaked: a shared counter
///   written on every hit, which every core then contends on.
///
/// The safe reference version of each table is in the tests, which check both give the same
/// rows and values.
pub(crate) mod table {
    use std::{
        alloc::{self, Layout},
        hash::{BuildHasher, Hash},
        marker::PhantomData,
        mem::ManuallyDrop,
        num::NonZeroU32,
        ptr,
        sync::{
            Mutex, MutexGuard, PoisonError,
            atomic::{AtomicPtr, AtomicU8, AtomicU32, AtomicU64, Ordering},
        },
    };

    use rustc_hash::FxBuildHasher;
    use rustc_type_ir::{DebruijnIndex, TypeFlags};

    /// A type a table holds under its `'static` name: a table is a `static`, and every value in
    /// it lives as long.
    ///
    /// # Safety
    ///
    /// `Self::Static` is `Self` with each lifetime `'static`, so the two have one layout and the
    /// same valid values; and `Self`'s lifetime bounds no reference in it (the next solver's
    /// `'db` marks its handles, which are `PhantomData` and an index), so a value stays valid
    /// however long the table keeps it.
    pub(crate) unsafe trait Erase: Sized {
        type Static: 'static;
    }

    /// [`Erase`] for each `$name<'db>`, a kind whose one lifetime is `'db`.
    macro_rules! erase {
        ($($name:ident),+ $(,)?) => {$(
            // SAFETY: `$name<'static>` is `$name<'db>` with its one lifetime `'static`, which
            // bounds no reference in it (`table::Erase`).
            unsafe impl<'db> $crate::next_solver::interner::table::Erase for $name<'db> {
                type Static = $name<'static>;
            }
        )+};
    }
    pub(crate) use erase;

    /// What [`Erase`] promises of `V`, checked where the compiler can.
    #[inline(always)]
    fn same_layout<V: Erase>() {
        const {
            assert!(size_of::<V>() == size_of::<V::Static>());
            assert!(align_of::<V>() == align_of::<V::Static>());
        }
    }

    /// `value` under its `'static` name.
    #[inline]
    fn erase<V: Erase>(value: V) -> V::Static {
        same_layout::<V>();
        let value = ManuallyDrop::new(value);
        // SAFETY: `V::Static` is `V` but for lifetimes (`Erase`), and `value` is moved, not
        // dropped.
        unsafe { ptr::read(ptr::from_ref::<V>(&value).cast::<V::Static>()) }
    }

    /// `value` under its name for the caller's lifetime.
    #[inline]
    fn unerase<V: Erase>(value: V::Static) -> V {
        same_layout::<V>();
        let value = ManuallyDrop::new(value);
        // SAFETY: as in `erase`.
        unsafe { ptr::read(ptr::from_ref::<V::Static>(&value).cast::<V>()) }
    }

    #[inline]
    fn unerase_ref<V: Erase>(value: &V::Static) -> &V {
        same_layout::<V>();
        // SAFETY: as in `erase`; the reference keeps its lifetime.
        unsafe { &*ptr::from_ref(value).cast::<V>() }
    }

    /// `value` under its name for the caller's lifetime, to change in place: what is written
    /// through it is a `V`, whose lifetimes bound no reference in it, so it stays valid kept
    /// under the `'static` name.
    #[inline]
    pub(crate) fn unerase_mut<V: Erase>(value: &mut V::Static) -> &mut V {
        same_layout::<V>();
        // SAFETY: as in `erase`; the reference keeps its lifetime, and a `V` written through it
        // is a valid `V::Static` (`Erase`).
        unsafe { &mut *ptr::from_mut(value).cast::<V>() }
    }

    #[inline]
    fn erase_slice<V: Erase>(items: &[V]) -> &[V::Static] {
        same_layout::<V>();
        // SAFETY: as in `erase`, for each item; the slice keeps its lifetime.
        unsafe { std::slice::from_raw_parts(items.as_ptr().cast(), items.len()) }
    }

    #[inline]
    fn unerase_slice<V: Erase>(items: &[V::Static]) -> &[V] {
        same_layout::<V>();
        // SAFETY: as in `erase_slice`.
        unsafe { std::slice::from_raw_parts(items.as_ptr().cast(), items.len()) }
    }

    #[cold]
    #[inline(never)]
    fn never_written(what: &str, row: NonZeroU32) -> ! {
        panic!("row {row} of the {what} table was never written")
    }

    /// The last row of a table whose rows a [`GenericArg`](super::GenericArg) packs with a
    /// 2-bit tag in a `u32`: types, regions and constants, and so every table, alike.
    pub(crate) const LAST_ROW: u32 = (1 << 30) - 1;

    /// The first segment of a column holds 2^10 values, each next one twice as many, so 23
    /// segments hold all 2^32 positions.
    const FIRST_BITS: u32 = 10;
    const SEGMENTS: usize = 23;

    const fn segment_start(segment: usize) -> u64 {
        ((1 << segment) - 1) << FIRST_BITS
    }

    const fn segment_len(segment: usize) -> u64 {
        1 << (FIRST_BITS as usize + segment)
    }

    /// The segment position `at` is in, and where in it.
    #[inline]
    fn locate(at: u64) -> (usize, usize) {
        let segment = (63 - ((at >> FIRST_BITS) + 1).leading_zeros()) as usize;
        (segment, (at - segment_start(segment)) as usize)
    }

    /// Values by position, appended and never moved or freed: segments allocated on first use,
    /// so a reference into one lives as long as the column (a `static`). Each position is
    /// written once, by the thread that claimed it, before anything naming it is published by
    /// a release store that the reader's acquire load pairs with.
    struct Column<T> {
        segments: [AtomicPtr<T>; SEGMENTS],
        /// Whether segments start zeroed: a column of atomics that are read before they are
        /// written.
        zeroed: bool,
        of: PhantomData<T>,
    }

    impl<T> Column<T> {
        const fn new() -> Column<T> {
            Column::with(false)
        }

        /// A column of atomics whose all-zero bits are a value: what a position not written yet
        /// reads as.
        const fn zeroed() -> Column<T> {
            Column::with(true)
        }

        const fn with(zeroed: bool) -> Column<T> {
            Column {
                segments: [const { AtomicPtr::new(ptr::null_mut()) }; SEGMENTS],
                zeroed,
                of: PhantomData,
            }
        }

        /// Where segment `segment` starts, allocated if this is its first use.
        #[inline]
        fn segment(&self, segment: usize) -> *mut T {
            let base = self.segments[segment].load(Ordering::Acquire);
            if base.is_null() { self.allocate(segment) } else { base }
        }

        #[cold]
        fn allocate(&self, segment: usize) -> *mut T {
            let layout = Layout::array::<T>(segment_len(segment) as usize)
                .expect("a column segment fits in memory");
            assert!(layout.size() > 0, "a column of zero-sized values");
            let fresh = match self.zeroed {
                // SAFETY: `layout` is not zero-sized (asserted above).
                true => unsafe { alloc::alloc_zeroed(layout) },
                // SAFETY: as above.
                false => unsafe { alloc::alloc(layout) },
            }
            .cast::<T>();
            if fresh.is_null() {
                alloc::handle_alloc_error(layout);
            }
            match self.segments[segment].compare_exchange(
                ptr::null_mut(),
                fresh,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => fresh,
                Err(won) => {
                    // SAFETY: `fresh` was allocated above with `layout` and lost the race to be
                    // published, so no other thread has seen it.
                    unsafe { alloc::dealloc(fresh.cast(), layout) };
                    won
                }
            }
        }

        /// Writes `value` at `at`.
        ///
        /// # Safety
        ///
        /// `at` is below 2^32, claimed by the calling thread alone and not written yet.
        #[inline]
        unsafe fn write(&self, at: u64, value: T) {
            let (segment, offset) = locate(at);
            let slot = self.segment(segment).wrapping_add(offset);
            // SAFETY: `slot` is in its segment (`locate`), allocated and never freed, and the
            // caller alone writes it (the caller's precondition).
            unsafe { slot.write(value) }
        }

        /// The value at `at`, of a zeroed column, its segment allocated if need be: a position
        /// to store into.
        ///
        /// # Safety
        ///
        /// The column is [`Column::zeroed`] and `T` is an atomic, and `at` is below 2^32.
        #[inline]
        unsafe fn place(&self, at: u64) -> &T {
            let (segment, offset) = locate(at);
            let slot = self.segment(segment).wrapping_add(offset);
            // SAFETY: `slot` is in its segment, allocated zeroed, which is a `T` (the caller's
            // precondition), and never freed.
            unsafe { &*slot }
        }

        /// The value at `at` of a zeroed column, `None` when its segment was never allocated
        /// (so it was never written).
        ///
        /// # Safety
        ///
        /// As [`Column::place`].
        #[inline]
        unsafe fn peek(&self, at: u64) -> Option<&T> {
            let (segment, offset) = locate(at);
            let base = self.segments[segment].load(Ordering::Acquire);
            // SAFETY: as in `place`; the acquire load pairs with `allocate`'s publication of
            // the zeroed segment.
            (!base.is_null()).then(|| unsafe { &*base.wrapping_add(offset) })
        }

        /// The value at `at`.
        ///
        /// # Safety
        ///
        /// `at` was written (or the column is zeroed and `T` an atomic), and that write happens
        /// before this call. The write allocated the segment, so the relaxed load of its address
        /// sees it.
        #[inline]
        unsafe fn get(&self, at: u64) -> &T {
            let (segment, offset) = locate(at);
            let slot = self.segments[segment].load(Ordering::Relaxed).wrapping_add(offset);
            // SAFETY: the caller's precondition; segments are never freed.
            unsafe { &*slot }
        }

        /// The `len` values from `start`.
        ///
        /// # Safety
        ///
        /// As [`Column::get`] for each of them, and they are in one segment, as [`Run::claim`]
        /// gives them.
        #[inline]
        unsafe fn slice(&self, start: u64, len: usize) -> &[T] {
            if len == 0 {
                return &[];
            }
            let (segment, offset) = locate(start);
            let first = self.segments[segment].load(Ordering::Relaxed).wrapping_add(offset);
            // SAFETY: the caller's precondition.
            unsafe { std::slice::from_raw_parts(first, len) }
        }
    }

    /// A column whose positions are claimed in runs that never cross a segment, so that a run
    /// is one slice.
    struct Run<T> {
        column: Column<T>,
        next: AtomicU64,
    }

    impl<T> Run<T> {
        const fn new() -> Run<T> {
            Run { column: Column::new(), next: AtomicU64::new(0) }
        }

        /// Claims `len` (not 0) positions in a row for the calling thread, returning the first.
        /// Panics, naming `what`, past 2^32 positions.
        fn claim(&self, len: u64, what: &str) -> u64 {
            let mut next = self.next.load(Ordering::Relaxed);
            loop {
                let start = fit(next, len);
                let end = start + len;
                assert!(end <= 1 << 32, "the {what} table holds 2^32 words");
                match self.next.compare_exchange_weak(
                    next,
                    end,
                    Ordering::Relaxed,
                    Ordering::Relaxed,
                ) {
                    Ok(_) => return start,
                    Err(now) => next = now,
                }
            }
        }
    }

    /// The first position from `from` where `len` positions in a row are in one segment, or
    /// 2^32 when there is none.
    fn fit(from: u64, len: u64) -> u64 {
        let (mut segment, offset) = locate(from);
        if offset as u64 + len <= segment_len(segment) {
            return from;
        }
        loop {
            segment += 1;
            if segment >= SEGMENTS {
                return 1 << 32;
            }
            if len <= segment_len(segment) {
                return segment_start(segment);
            }
        }
    }

    /// The hash a value is found by: its Fx hash, mixed (murmur's finalizer) so that its top
    /// bits pick a shard and its low half a slot.
    #[inline]
    fn hash_of<T: Hash + ?Sized>(value: &T) -> u64 {
        let hash = FxBuildHasher.hash_one(value);
        let hash = (hash ^ (hash >> 33)).wrapping_mul(0xff51_afd7_ed55_8ccd);
        let hash = (hash ^ (hash >> 33)).wrapping_mul(0xc4ce_b9fe_1a85_ec53);
        hash ^ (hash >> 33)
    }

    const SHARD_BITS: u32 = 6;
    const SHARDS: usize = 1 << SHARD_BITS;
    const FIRST_SLOTS: usize = 16;

    /// Finds a row by its value's hash: 64 shards, each an open-addressed table (linear
    /// probing, at most half full) of `u64` slots, each the low half of a hash and a row (0 for
    /// an empty slot, as rows start at 1). A hit is loads alone; an insert takes the shard's
    /// lock, looks again and publishes the row with a release store. A table that grows is
    /// copied and the old one leaked, as a reader may still be probing it.
    struct Index {
        shards: [Shard; SHARDS],
    }

    struct Shard {
        /// Null until the first insert; replaced only by the holder of `len`.
        slots: AtomicPtr<Slots>,
        /// How many rows the shard holds.
        len: Mutex<usize>,
    }

    struct Slots {
        words: Box<[AtomicU64]>,
    }

    impl Slots {
        fn new(len: usize) -> Slots {
            Slots { words: (0..len).map(|_| AtomicU64::new(0)).collect() }
        }

        #[inline]
        fn find(&self, hash: u32, mut same: impl FnMut(NonZeroU32) -> bool) -> Option<NonZeroU32> {
            let mask = self.words.len() - 1;
            let mut at = hash as usize & mask;
            loop {
                let word = self.words[at].load(Ordering::Acquire);
                let row = NonZeroU32::new(word as u32)?;
                if (word >> 32) as u32 == hash && same(row) {
                    return Some(row);
                }
                at = (at + 1) & mask;
            }
        }

        /// Puts `word` in the first empty slot from its hash's. Only the holder of the shard's
        /// lock calls it, on a table with an empty slot.
        fn put(&self, word: u64) {
            let mask = self.words.len() - 1;
            let mut at = (word >> 32) as usize & mask;
            while self.words[at].load(Ordering::Relaxed) != 0 {
                at = (at + 1) & mask;
            }
            self.words[at].store(word, Ordering::Release);
        }
    }

    enum Entry<'a> {
        Found(NonZeroU32),
        Vacant(Vacant<'a>),
    }

    /// A hash not in the index, with its shard's lock held until a row is inserted for it.
    struct Vacant<'a> {
        shard: &'a Shard,
        len: MutexGuard<'a, usize>,
        hash: u32,
    }

    impl Index {
        const fn new() -> Index {
            Index {
                shards: [const {
                    Shard { slots: AtomicPtr::new(ptr::null_mut()), len: Mutex::new(0) }
                }; SHARDS],
            }
        }

        #[inline]
        fn shard(&self, hash: u64) -> &Shard {
            &self.shards[(hash >> (64 - SHARD_BITS)) as usize]
        }

        /// The row of hash `hash` that `same` accepts.
        #[inline]
        fn find(
            &self,
            hash: u64,
            same: impl FnMut(NonZeroU32) -> bool,
        ) -> Option<NonZeroU32> {
            let slots = self.shard(hash).slots.load(Ordering::Acquire);
            // SAFETY: a published table is never freed.
            unsafe { slots.as_ref() }?.find(hash as u32, same)
        }

        /// The row of hash `hash` that `same` accepts, looked for again under its shard's lock,
        /// or the lock to insert one.
        fn lock(&self, hash: u64, same: impl FnMut(NonZeroU32) -> bool) -> Entry<'_> {
            let shard = self.shard(hash);
            let len = shard.len.lock().unwrap_or_else(PoisonError::into_inner);
            match self.find(hash, same) {
                Some(row) => Entry::Found(row),
                None => Entry::Vacant(Vacant { shard, len, hash: hash as u32 }),
            }
        }
    }

    impl Vacant<'_> {
        /// Publishes `row`, written in full, for the hash, and releases the shard's lock.
        fn insert(mut self, row: NonZeroU32) {
            let len = *self.len + 1;
            let mut slots = self.shard.slots.load(Ordering::Relaxed);
            // SAFETY: a published table is never freed.
            let capacity = unsafe { slots.as_ref() }.map_or(0, |slots| slots.words.len());
            if len * 2 > capacity {
                let grown = Slots::new((capacity * 2).max(FIRST_SLOTS));
                // SAFETY: as above.
                if let Some(old) = unsafe { slots.as_ref() } {
                    for word in &old.words {
                        let word = word.load(Ordering::Relaxed);
                        if word != 0 {
                            grown.put(word);
                        }
                    }
                }
                slots = Box::into_raw(Box::new(grown));
                self.shard.slots.store(slots, Ordering::Release);
            }
            // SAFETY: `slots` is the shard's table, never freed.
            unsafe { &*slots }.put((u64::from(self.hash) << 32) | u64::from(row.get()));
            *self.len = len;
        }
    }

    /// What [`Rows::intern`] computes once, when it adds a value: its tag (its codec's first
    /// word, in `persist.rs`), flags and outer exclusive binder.
    #[derive(Clone, Copy)]
    pub(crate) struct RowInfo {
        pub(crate) tag: u8,
        pub(crate) flags: TypeFlags,
        pub(crate) binder: DebruijnIndex,
    }

    /// The rows of one kind with a structure, `K` its kind (`TyKind<'static>`, ...), kept whole
    /// so that `kind()` is one indexed copy, as cheap as rustc's pointer dereference. Columns,
    /// hot apart from cold:
    ///
    /// - `flags` and `binder`, what every `has_type_flags`/`has_escaping_bound_vars` reads;
    /// - `tag`, the kind's tag plus one (0 for a row not written yet, so storing it publishes
    ///   the row): a scan by kind reads one byte a row;
    /// - `kinds`, the value, read by `kind()` and by the index's equality check.
    ///
    /// Lookup: one hash of the kind (no encoding), one probe of the index (a cache line), one
    /// equality check of the row it names.
    pub(crate) struct Rows<K> {
        what: &'static str,
        last: u32,
        next: AtomicU64,
        tag: Column<AtomicU8>,
        flags: Column<u32>,
        binder: Column<u32>,
        kinds: Column<K>,
        index: Index,
    }

    /// The bytes a row takes in its hot columns (`tag`, `flags`, `binder`), its kind aside.
    pub(crate) const ROW_BYTES: usize =
        size_of::<AtomicU8>() + size_of::<u32>() + size_of::<u32>();
    const _: () = assert!(ROW_BYTES == 9);

    impl<K: Copy + Hash + Eq> Rows<K> {
        /// A table of rows 1 to `last`, named `what` in what it panics with.
        pub(crate) const fn new(what: &'static str, last: u32) -> Rows<K> {
            Rows {
                what,
                last,
                next: AtomicU64::new(1),
                tag: Column::zeroed(),
                flags: Column::new(),
                binder: Column::new(),
                kinds: Column::new(),
                index: Index::new(),
            }
        }

        #[inline]
        pub(crate) fn name(&self) -> &'static str {
            self.what
        }

        /// The row of `kind`, added with what `info` computes if it is not there. A hit writes
        /// nothing and allocates nothing.
        #[inline]
        pub(crate) fn intern<V: Erase<Static = K>>(
            &self,
            kind: V,
            info: impl FnOnce() -> RowInfo,
        ) -> NonZeroU32 {
            let kind = erase(kind);
            let hash = hash_of(&kind);
            match self.index.find(hash, |row| self.indexed(row) == kind) {
                Some(row) => row,
                None => self.add(hash, kind, info),
            }
        }

        /// The kind of a row the index names.
        #[inline]
        fn indexed(&self, row: NonZeroU32) -> K {
            // SAFETY: the index names only rows written in full: its acquire load of a slot
            // pairs with the release store `Vacant::insert` makes after `add`'s writes.
            unsafe { *self.kinds.get(row.get().into()) }
        }

        #[cold]
        fn add(&self, hash: u64, kind: K, info: impl FnOnce() -> RowInfo) -> NonZeroU32 {
            let info = info();
            assert!(info.tag < u8::MAX, "a {} tag of {}", self.what, info.tag);
            let vacant = match self.index.lock(hash, |row| self.indexed(row) == kind) {
                Entry::Found(row) => return row,
                Entry::Vacant(vacant) => vacant,
            };
            let row = self.next.fetch_add(1, Ordering::Relaxed);
            assert!(row <= u64::from(self.last), "the {} table holds {} rows", self.what, self.last);
            // SAFETY: `row` is ours alone (claimed above), below 2^32 and not written yet.
            unsafe { self.kinds.write(row, kind) };
            // SAFETY: as above.
            unsafe { self.flags.write(row, info.flags.bits()) };
            // SAFETY: as above.
            unsafe { self.binder.write(row, info.binder.as_u32()) };
            // SAFETY: the tag column is zeroed and of atomics, and `row` is below 2^32.
            let tag = unsafe { self.tag.place(row) };
            // Publishes the row to `tag_of`'s acquire load.
            tag.store(info.tag + 1, Ordering::Release);
            let row = NonZeroU32::new(row as u32).expect("rows start at 1");
            vacant.insert(row);
            row
        }

        /// Whether `row` is a row written in full: what a handle made from a bare index checks.
        pub(crate) fn published(&self, row: NonZeroU32) -> bool {
            // SAFETY: the tag column is zeroed and of atomics, and a row is below 2^32.
            let tag = unsafe { self.tag.peek(row.get().into()) };
            tag.is_some_and(|tag| tag.load(Ordering::Acquire) != 0)
        }

        /// The tag of `row` plus one. Panics unless the row was written in full; when it was,
        /// the acquire load pairs with the release store that published it, so every column of
        /// the row reads as written.
        #[inline]
        fn tag_of(&self, row: NonZeroU32) -> u8 {
            // SAFETY: the tag column is zeroed and of atomics, and a row is below 2^32.
            let tag = unsafe { self.tag.peek(row.get().into()) };
            match tag.map_or(0, |tag| tag.load(Ordering::Acquire)) {
                0 => never_written(self.what, row),
                tag => tag,
            }
        }

        /// The kind of `row`. Panics if it is not a row of this table.
        #[inline]
        pub(crate) fn kind<V: Erase<Static = K>>(&self, row: NonZeroU32) -> V {
            self.tag_of(row);
            // SAFETY: `tag_of` checked the row was written and published.
            unerase(unsafe { *self.kinds.get(row.get().into()) })
        }

        /// The tag of `row`'s kind.
        #[inline]
        pub(crate) fn tag(&self, row: NonZeroU32) -> u8 {
            self.tag_of(row) - 1
        }

        /// The flags of `row`.
        #[inline]
        pub(crate) fn flags(&self, row: NonZeroU32) -> TypeFlags {
            self.tag_of(row);
            // SAFETY: as in `kind`.
            TypeFlags::from_bits_retain(unsafe { *self.flags.get(row.get().into()) })
        }

        /// The outer exclusive binder of `row`.
        #[inline]
        pub(crate) fn binder(&self, row: NonZeroU32) -> DebruijnIndex {
            self.tag_of(row);
            // SAFETY: as in `kind`.
            DebruijnIndex::from_u32(unsafe { *self.binder.get(row.get().into()) })
        }
    }

    /// Interned lists of `T`: each row a start and a length over one column of items. A list's
    /// items are one run, so a list is one slice. The length column holds the length plus one,
    /// 0 for a row not written yet, and storing it publishes the row. For a kind whose lists
    /// have flags (clauses), the flags and outer exclusive binder of the whole list are columns
    /// too: one load tests a block of items.
    pub(crate) struct Lists<T> {
        what: &'static str,
        /// Whether the lists have flags: [`Lists::with_flags`], all interned by
        /// [`Lists::intern_with`].
        flagged: bool,
        next: AtomicU64,
        start: Column<u32>,
        len: Column<AtomicU32>,
        flags: Column<u32>,
        binder: Column<u32>,
        items: Run<T>,
        index: Index,
    }

    /// The bytes a list takes in its columns, its items aside (and the flag columns, for the
    /// lists that have them).
    pub(crate) const LIST_BYTES: usize = size_of::<u32>() + size_of::<AtomicU32>();
    const _: () = assert!(LIST_BYTES == 8);

    impl<T: Copy + Hash + Eq> Lists<T> {
        /// A table of lists without flags, interned by [`Lists::intern`].
        pub(crate) const fn new(what: &'static str) -> Lists<T> {
            Lists::make(what, false)
        }

        /// A table of lists with flags, interned by [`Lists::intern_with`].
        pub(crate) const fn with_flags(what: &'static str) -> Lists<T> {
            Lists::make(what, true)
        }

        const fn make(what: &'static str, flagged: bool) -> Lists<T> {
            Lists {
                what,
                flagged,
                next: AtomicU64::new(1),
                start: Column::new(),
                len: Column::zeroed(),
                flags: Column::new(),
                binder: Column::new(),
                items: Run::new(),
                index: Index::new(),
            }
        }

        #[inline]
        pub(crate) fn name(&self) -> &'static str {
            self.what
        }

        /// The row of the list `items`, added if it is not there. Panics on a table
        /// [`Lists::with_flags`].
        pub(crate) fn intern<V: Erase<Static = T>>(&self, items: &[V]) -> NonZeroU32 {
            self.put(erase_slice(items), || None)
        }

        /// As [`Lists::intern`], for a table [`Lists::with_flags`]: `info` computes them.
        pub(crate) fn intern_with<V: Erase<Static = T>>(
            &self,
            items: &[V],
            info: impl FnOnce() -> (TypeFlags, DebruijnIndex),
        ) -> NonZeroU32 {
            self.put(erase_slice(items), || Some(info()))
        }

        #[inline]
        fn put(
            &self,
            items: &[T],
            info: impl FnOnce() -> Option<(TypeFlags, DebruijnIndex)>,
        ) -> NonZeroU32 {
            let hash = hash_of(items);
            match self.index.find(hash, |row| self.items(row) == items) {
                Some(row) => row,
                None => self.add(hash, items, info),
            }
        }

        #[cold]
        fn add(
            &self,
            hash: u64,
            items: &[T],
            info: impl FnOnce() -> Option<(TypeFlags, DebruijnIndex)>,
        ) -> NonZeroU32 {
            let info = info();
            assert_eq!(info.is_some(), self.flagged, "the flags of a list of {}", self.what);
            let vacant = match self.index.lock(hash, |row| self.items(row) == items) {
                Entry::Found(row) => return row,
                Entry::Vacant(vacant) => vacant,
            };
            let len = u32::try_from(items.len() + 1)
                .unwrap_or_else(|_| panic!("a list of {} items in {}", items.len(), self.what));
            let row = self.next.fetch_add(1, Ordering::Relaxed);
            assert!(row <= u64::from(LAST_ROW), "the {} table holds {LAST_ROW} rows", self.what);
            let start = match items.is_empty() {
                true => 0,
                false => self.items.claim(items.len() as u64, self.what),
            };
            for (i, &item) in items.iter().enumerate() {
                // SAFETY: the positions `claim` gave are ours alone, below 2^32 and not written
                // yet.
                unsafe { self.items.column.write(start + i as u64, item) };
            }
            // SAFETY: `row` is ours alone (claimed above), below 2^32 and not written yet.
            unsafe { self.start.write(row, start as u32) };
            if let Some((flags, binder)) = info {
                // SAFETY: as above.
                unsafe { self.flags.write(row, flags.bits()) };
                // SAFETY: as above.
                unsafe { self.binder.write(row, binder.as_u32()) };
            }
            // SAFETY: the length column is zeroed and of atomics, and `row` is below 2^32.
            let slot = unsafe { self.len.place(row) };
            // Publishes the row to `written`'s acquire load.
            slot.store(len, Ordering::Release);
            let row = NonZeroU32::new(row as u32).expect("rows start at 1");
            vacant.insert(row);
            row
        }

        /// Whether `row` is a list written in full: what a handle made from a bare index checks.
        pub(crate) fn published(&self, row: NonZeroU32) -> bool {
            // SAFETY: the length column is zeroed and of atomics, and a row is below 2^32.
            let len = unsafe { self.len.peek(row.get().into()) };
            len.is_some_and(|len| len.load(Ordering::Acquire) != 0)
        }

        /// The length of `row`. Panics unless the row was written in full; when it was, the
        /// acquire load pairs with the release store that published it, so every column of the
        /// row, and its items, read as written.
        #[inline]
        fn written(&self, row: NonZeroU32) -> usize {
            // SAFETY: the length column is zeroed and of atomics, and a row is below 2^32.
            let len = unsafe { self.len.peek(row.get().into()) };
            match len.map_or(0, |len| len.load(Ordering::Acquire)) {
                0 => never_written(self.what, row),
                len => len as usize - 1,
            }
        }

        #[inline]
        fn items(&self, row: NonZeroU32) -> &[T] {
            let len = self.written(row);
            // SAFETY: `written` checked the row was written and published.
            let start = unsafe { *self.start.get(row.get().into()) };
            // SAFETY: as above, for its items, which `Run::claim` put in one segment.
            unsafe { self.items.column.slice(u64::from(start), len) }
        }

        /// The items of `row`. Panics if it is not a row of this table.
        #[inline]
        pub(crate) fn get<V: Erase<Static = T>>(&self, row: NonZeroU32) -> &[V] {
            unerase_slice(self.items(row))
        }

        /// The flags of `row`. Panics if it is not a row of this table, or the table has no
        /// flags.
        #[inline]
        pub(crate) fn flags(&self, row: NonZeroU32) -> TypeFlags {
            self.flagged_row(row);
            // SAFETY: the row was written and published (`written`), with its flags, as the
            // table's every list has them (`add`'s assertion).
            TypeFlags::from_bits_retain(unsafe { *self.flags.get(row.get().into()) })
        }

        /// The outer exclusive binder of `row`, as [`Lists::flags`].
        #[inline]
        pub(crate) fn binder(&self, row: NonZeroU32) -> DebruijnIndex {
            self.flagged_row(row);
            // SAFETY: as in `flags`.
            DebruijnIndex::from_u32(unsafe { *self.binder.get(row.get().into()) })
        }

        #[inline]
        fn flagged_row(&self, row: NonZeroU32) {
            assert!(self.flagged, "the lists of {} have no flags", self.what);
            self.written(row);
        }
    }

    /// Interned values kept whole, for the kinds whose API hands out a reference to the value.
    /// The mark column is 1 for a row written in full, and storing it publishes the row.
    pub(crate) struct Values<T> {
        what: &'static str,
        next: AtomicU64,
        marks: Column<AtomicU8>,
        values: Column<T>,
        index: Index,
    }

    impl<T: Hash + Eq> Values<T> {
        pub(crate) const fn new(what: &'static str) -> Values<T> {
            Values {
                what,
                next: AtomicU64::new(1),
                marks: Column::zeroed(),
                values: Column::new(),
                index: Index::new(),
            }
        }

        #[inline]
        pub(crate) fn name(&self) -> &'static str {
            self.what
        }

        /// The row of `value`, added if it is not there.
        pub(crate) fn intern<V: Erase<Static = T>>(&self, value: V) -> NonZeroU32 {
            let value = erase(value);
            let hash = hash_of(&value);
            if let Some(row) = self.index.find(hash, |row| self.indexed(row) == &value) {
                return row;
            }
            let vacant = match self.index.lock(hash, |row| self.indexed(row) == &value) {
                Entry::Found(row) => return row,
                Entry::Vacant(vacant) => vacant,
            };
            let row = self.next.fetch_add(1, Ordering::Relaxed);
            assert!(row <= u64::from(LAST_ROW), "the {} table holds {LAST_ROW} rows", self.what);
            // SAFETY: `row` is ours alone (claimed above), below 2^32 and not written yet.
            unsafe { self.values.write(row, value) };
            // SAFETY: the mark column is zeroed and of atomics, and `row` is below 2^32.
            let mark = unsafe { self.marks.place(row) };
            // Publishes the row to `get`'s acquire load.
            mark.store(1, Ordering::Release);
            let row = NonZeroU32::new(row as u32).expect("rows start at 1");
            vacant.insert(row);
            row
        }

        /// The value of a row the index names.
        #[inline]
        fn indexed(&self, row: NonZeroU32) -> &T {
            // SAFETY: as in `Rows::indexed`.
            unsafe { self.values.get(row.get().into()) }
        }

        /// The value of `row`. Panics if it is not a row of this table.
        #[inline]
        pub(crate) fn get<V: Erase<Static = T>>(&self, row: NonZeroU32) -> &V {
            // SAFETY: the mark column is zeroed and of atomics, and a row is below 2^32.
            let mark = unsafe { self.marks.peek(row.get().into()) };
            if mark.is_none_or(|mark| mark.load(Ordering::Acquire) == 0) {
                never_written(self.what, row);
            }
            // SAFETY: the row was written, and the acquire load of its mark pairs with the
            // release store that published it.
            unerase_ref(unsafe { self.values.get(row.get().into()) })
        }
    }

    #[cfg(test)]
    mod tests {
        use std::{
            collections::{HashMap, HashSet},
            hash::Hash,
            num::NonZeroU32,
            sync::Barrier,
        };

        use rustc_type_ir::{DebruijnIndex, INNERMOST, TypeFlags};

        use super::{
            Erase, FIRST_BITS, Lists, RowInfo, Rows, SEGMENTS, Values, fit, locate, segment_len,
            segment_start,
        };

        macro_rules! erase_static {
            ($($ty:ty),+) => {$(
                // SAFETY: `$ty` has no lifetime: `Static` is `$ty` itself.
                unsafe impl Erase for $ty {
                    type Static = $ty;
                }
            )+};
        }
        erase_static!(u32, u64, (u32, u64), [u32; 3], String);

        fn tagged(tag: u32) -> impl FnOnce() -> RowInfo {
            move || RowInfo { tag: tag as u8, flags: TypeFlags::empty(), binder: INNERMOST }
        }

        /// The safe reference version of a table: a map from value to row under one lock, rows
        /// dense from 1 in the order values are first seen, as the tables give them to one
        /// thread.
        struct Reference<K> {
            rows: HashMap<K, NonZeroU32>,
            values: Vec<K>,
        }

        impl<K: Clone + Hash + Eq> Reference<K> {
            fn new() -> Reference<K> {
                Reference { rows: HashMap::new(), values: Vec::new() }
            }

            fn intern(&mut self, value: K) -> NonZeroU32 {
                let next = NonZeroU32::new(self.values.len() as u32 + 1).unwrap();
                *self.rows.entry(value.clone()).or_insert_with(|| {
                    self.values.push(value);
                    next
                })
            }

            fn get(&self, row: NonZeroU32) -> &K {
                &self.values[row.get() as usize - 1]
            }
        }

        /// A sequence of pseudo-random numbers below `below` (xorshift), with repeats.
        fn numbers(seed: u64, count: usize, below: u64) -> impl Iterator<Item = u64> {
            let mut state = seed;
            std::iter::repeat_with(move || {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                state % below
            })
            .take(count)
        }

        #[test]
        fn each_table_gives_the_rows_and_values_of_its_safe_reference() {
            static ROWS: Rows<(u32, u64)> = Rows::new("reference rows", 1 << 20);
            static LISTS: Lists<u32> = Lists::new("reference lists");
            static VALUES: Values<String> = Values::new("reference values");
            let mut rows = Reference::new();
            let mut lists = Reference::new();
            let mut values = Reference::new();
            for n in numbers(0x9e37_79b9_7f4a_7c15, 20_000, 3000) {
                let kind = ((n % 7) as u32, n * 0x1_0000_0001);
                let row = ROWS.intern(kind, tagged(kind.0));
                assert_eq!(row, rows.intern(kind));
                assert_eq!(ROWS.kind::<(u32, u64)>(row), *rows.get(row));
                assert_eq!(u32::from(ROWS.tag(row)), kind.0);

                // Lengths up to 2500 cross the first segments' ends.
                let items: Vec<u32> = (0..(n * 13 % 2500) as u32).map(|i| i ^ n as u32).collect();
                let row = LISTS.intern(&items[..]);
                assert_eq!(row, lists.intern(items.clone()));
                assert_eq!(LISTS.get::<u32>(row), &lists.get(row)[..]);

                let value = format!("value {}", n % 500);
                let row = VALUES.intern(value.clone());
                assert_eq!(row, values.intern(value));
                assert_eq!(VALUES.get::<String>(row), values.get(row));
            }
            for (row, kind) in rows.values.iter().enumerate() {
                let row = NonZeroU32::new(row as u32 + 1).unwrap();
                assert_eq!(ROWS.kind::<(u32, u64)>(row), *kind);
            }
        }

        #[test]
        #[should_panic(expected = "row 3 of the unwritten rows table was never written")]
        fn reading_a_row_not_written_panics() {
            static ROWS: Rows<u32> = Rows::new("unwritten rows", 10);
            ROWS.intern(1u32, tagged(0));
            ROWS.intern(2u32, tagged(0));
            ROWS.flags(NonZeroU32::new(3).unwrap());
        }

        #[test]
        #[should_panic(expected = "row 2 of the unwritten lists table was never written")]
        fn reading_a_list_not_written_panics() {
            static LISTS: Lists<u32> = Lists::new("unwritten lists");
            LISTS.intern(&[1u32][..]);
            LISTS.get::<u32>(NonZeroU32::new(2).unwrap());
        }

        #[test]
        #[should_panic(expected = "row 1 of the unwritten values table was never written")]
        fn reading_a_value_not_written_panics() {
            static VALUES: Values<u64> = Values::new("unwritten values");
            VALUES.get::<u64>(NonZeroU32::new(1).unwrap());
        }

        #[test]
        #[should_panic(expected = "the lists of lists without flags have no flags")]
        fn the_flags_of_a_table_without_them_panic() {
            static LISTS: Lists<u32> = Lists::new("lists without flags");
            let row = LISTS.intern(&[1u32][..]);
            LISTS.flags(row);
        }

        #[test]
        #[should_panic(expected = "the flags of a list of lists with flags")]
        fn a_list_without_flags_in_a_table_with_them_panics() {
            static LISTS: Lists<u32> = Lists::with_flags("lists with flags");
            LISTS.intern(&[1u32][..]);
        }

        #[test]
        fn a_list_keeps_its_flags() {
            static LISTS: Lists<u32> = Lists::with_flags("flagged lists");
            let row = LISTS.intern_with(&[4u32, 5][..], || {
                (TypeFlags::HAS_TY_INFER, DebruijnIndex::from_u32(2))
            });
            assert_eq!(LISTS.flags(row), TypeFlags::HAS_TY_INFER);
            assert_eq!(LISTS.binder(row), DebruijnIndex::from_u32(2));
            assert_eq!(LISTS.get::<u32>(row), &[4, 5]);
        }

        #[test]
        fn positions_are_in_the_segment_that_holds_them() {
            for segment in 0..SEGMENTS {
                let start = segment_start(segment);
                let end = start + segment_len(segment) - 1;
                assert_eq!(locate(start), (segment, 0));
                assert_eq!(locate(end), (segment, (segment_len(segment) - 1) as usize));
            }
            assert!(segment_start(SEGMENTS - 1) + segment_len(SEGMENTS - 1) >= 1 << 32);
            assert_eq!(segment_len(0), 1 << FIRST_BITS);
        }

        #[test]
        fn a_run_never_crosses_a_segment() {
            assert_eq!(fit(0, 10), 0);
            assert_eq!(fit(1020, 4), 1020);
            assert_eq!(fit(1021, 4), 1024);
            assert_eq!(fit(1021, 3000), segment_start(2));
            assert_eq!(fit(0, 1 << 33), 1 << 32);
        }

        #[test]
        fn a_row_interned_twice_is_one_row() {
            static ROWS: Rows<(u32, u64)> = Rows::new("test rows", 1000);
            let values = [(0, 0), (0, 7), (1, 7), (1, u64::MAX), (3, 1 << 40)];
            let rows: Vec<_> = values.iter().map(|&kind| ROWS.intern(kind, tagged(kind.0))).collect();
            for (&kind, &row) in values.iter().zip(&rows) {
                assert_eq!(ROWS.intern(kind, || panic!("computed the info of a value interned")), row);
                assert_eq!(ROWS.kind::<(u32, u64)>(row), kind);
                assert_eq!(u32::from(ROWS.tag(row)), kind.0);
                assert!(ROWS.published(row));
            }
            let distinct: HashSet<_> = rows.iter().collect();
            assert_eq!(distinct.len(), values.len());
            assert!(!ROWS.published(NonZeroU32::new(999).unwrap()));
        }

        #[test]
        fn a_row_keeps_its_flags() {
            static ROWS: Rows<u32> = Rows::new("test rows with flags", 10);
            let row = ROWS.intern(5u32, || RowInfo {
                tag: 2,
                flags: TypeFlags::HAS_TY_PARAM | TypeFlags::HAS_ERROR,
                binder: DebruijnIndex::from_u32(3),
            });
            assert_eq!(ROWS.flags(row), TypeFlags::HAS_TY_PARAM | TypeFlags::HAS_ERROR);
            assert_eq!(ROWS.binder(row), DebruijnIndex::from_u32(3));
            assert_eq!(ROWS.tag(row), 2);
        }

        #[test]
        #[should_panic(expected = "the tiny table holds 2 rows")]
        fn a_full_table_says_so() {
            static ROWS: Rows<u32> = Rows::new("tiny", 2);
            for value in 0u32..3 {
                ROWS.intern(value, tagged(0));
            }
        }

        #[test]
        fn a_list_interned_twice_is_one_list() {
            static LISTS: Lists<u64> = Lists::new("test lists");
            let lists: [&[u64]; 4] = [&[], &[1], &[1, 2], &[2, 1]];
            let rows: Vec<_> = lists.iter().map(|&items| LISTS.intern(items)).collect();
            for (items, &row) in lists.iter().zip(&rows) {
                assert_eq!(LISTS.intern(*items), row);
                assert_eq!(LISTS.get::<u64>(row), *items);
                assert!(LISTS.published(row));
            }
            assert_eq!(rows.iter().collect::<HashSet<_>>().len(), lists.len());
            // A list longer than a first segment's free room starts in a later segment.
            let long: Vec<u64> = (0..5000).collect();
            let row = LISTS.intern(&long[..]);
            assert_eq!(LISTS.get::<u64>(row), &long[..]);
        }

        #[test]
        fn a_value_interned_twice_is_one_value() {
            static VALUES: Values<String> = Values::new("test values");
            let a = VALUES.intern("a".to_owned());
            let b = VALUES.intern("b".to_owned());
            assert_ne!(a, b);
            assert_eq!(VALUES.intern("a".to_owned()), a);
            assert_eq!(VALUES.get::<String>(a), "a");
        }

        #[test]
        fn eight_threads_interning_at_once_get_one_row_per_value() {
            static ROWS: Rows<[u32; 3]> = Rows::new("parallel rows", 1 << 20);
            static LISTS: Lists<u32> = Lists::new("parallel lists");
            const THREADS: u32 = 8;
            const VALUES: u32 = 20_000;
            let barrier = Barrier::new(THREADS as usize);
            let seen: Vec<HashMap<u32, (NonZeroU32, NonZeroU32)>> = std::thread::scope(|scope| {
                let threads: Vec<_> = (0..THREADS)
                    .map(|thread| {
                        let barrier = &barrier;
                        scope.spawn(move || {
                            barrier.wait();
                            // Each thread interns every value, in an order of its own.
                            (0..VALUES)
                                .map(|i| (i * 7919 + thread * 104_729) % VALUES)
                                .map(|value| {
                                    let words = [value % 100, value, value / 3];
                                    let row = ROWS.intern(words, tagged(value % 100));
                                    let list = LISTS.intern(&words[..1 + value as usize % 3]);
                                    (value, (row, list))
                                })
                                .collect()
                        })
                    })
                    .collect();
                threads.into_iter().map(|thread| thread.join().unwrap()).collect()
            });
            for value in 0..VALUES {
                let first = seen[0][&value];
                assert!(seen.iter().all(|thread| thread[&value] == first), "value {value}");
            }
            let rows: HashSet<_> = seen[0].values().map(|it| it.0).collect();
            assert_eq!(rows.len(), VALUES as usize);
            for (&value, &(row, list)) in &seen[0] {
                let words = [value % 100, value, value / 3];
                assert_eq!(ROWS.kind::<[u32; 3]>(row), words);
                assert_eq!(LISTS.get::<u32>(list), &words[..1 + value as usize % 3]);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use hir_def::{LifetimeParamId, TypeParamId};
    use rustc_type_ir::{
        CanonicalParamEnvCacheEntry, CanonicalQueryInput, OutlivesClause, TypingMode,
        TypingModeEqWrapper, UniverseIndex, solve::QueryInput,
    };

    use super::*;
    use crate::next_solver::{CanonicalVarKinds, Goal, PredicateKind, default_types};

    fn function() -> GenericDefId {
        GenericDefId::FunctionId(salsa::plumbing::FromId::from_id(salsa::Id::from_bits(1)))
    }

    fn early(index: u32) -> Region<'static> {
        let id = LifetimeParamId {
            parent: function(),
            local_id: la_arena::Idx::from_raw(la_arena::RawIdx::from_u32(index)),
        };
        rustc_type_ir::Region(InternedRegion::intern(RegionKind::ReEarlyParam(EarlyParamRegion {
            id,
            index,
        })))
    }

    fn args(args: &[GenericArg<'static>]) -> GenericArgs<'static> {
        GenericArgs::new_from_slice(args)
    }

    fn input(universe: u32) -> CanonicalInputData<'static> {
        let predicate = Predicate::intern(Binder::dummy(PredicateKind::Ambiguous));
        CanonicalQueryInput {
            canonical: rustc_type_ir::Canonical {
                value: QueryInput {
                    goal: Goal { param_env: ParamEnv::empty(), predicate },
                    predefined_opaques_in_body: PredefinedOpaques::empty(),
                },
                max_universe: UniverseIndex::from_u32(universe),
                var_kinds: CanonicalVarKinds::empty(),
            },
            typing_mode: TypingModeEqWrapper(TypingMode::Coherence),
        }
    }

    #[test]
    fn a_canonical_input_is_one_row_and_reads_back() {
        let input_0 = CanonicalInput::new(input(0));
        assert_eq!(CanonicalInput::new(input(0)), input_0, "interned once");
        assert_ne!(CanonicalInput::new(input(1)), input_0);
        assert_eq!(*input_0, input(0));
    }

    #[test]
    fn the_keyword_symbols_are_rustcs() {
        type S = Symbol;
        type I = DbInterner<'static>;
        let kw = |symbol: S| symbol.0.as_str().to_owned();
        assert_eq!(kw(<S as inherent::Symbol<I>>::KW_UNDERSCORE_LIFETIME), "'_");
        assert_eq!(kw(<S as inherent::Symbol<I>>::KW_STATIC_LIFETIME), "'static");
        assert_eq!(kw(<S as inherent::Symbol<I>>::SYM_ANON), "anon");
    }

    #[test]
    fn args_fit_params_one_each_in_order() {
        let lifetime = GenericParamId::LifetimeParamId(LifetimeParamId {
            parent: function(),
            local_id: la_arena::Idx::from_raw(la_arena::RawIdx::from_u32(0)),
        });
        let ty = GenericParamId::TypeParamId(TypeParamId::from_unchecked(
            hir_def::TypeOrConstParamId {
                parent: function(),
                local_id: la_arena::Idx::from_raw(la_arena::RawIdx::from_u32(0)),
            },
        ));
        let statik: GenericArg<'static> = default_types().regions.statik.into();
        let u32_: GenericArg<'static> = default_types().types.u32.into();
        assert!(args_fit_params([lifetime, ty], &[statik, u32_]));
        assert!(!args_fit_params([lifetime, ty], &[u32_, statik]), "kinds out of order");
        assert!(!args_fit_params([lifetime, ty], &[statik]), "one missing");
        assert!(!args_fit_params([lifetime], &[statik, u32_]), "one too many");
        assert!(args_fit_params([], &[]));
    }

    #[test]
    fn reflection_takes_an_impl_generic_over_distinct_unconstrained_lifetimes() {
        let (a, b) = (early(0), early(1));
        let u32_ = default_types().types.u32;
        let statik = default_types().regions.statik;
        let outlives = |long: Region<'static>, short: Region<'static>| {
            Clause(Predicate::intern(Binder::dummy(PredicateKind::Clause(
                ClauseKind::RegionOutlives(OutlivesClause(long, short)),
            ))))
        };

        assert!(fully_generic_for_reflection(args(&[a.into(), b.into(), u32_.into()]), []));
        assert!(!fully_generic_for_reflection(args(&[a.into(), a.into()]), []), "a lifetime twice");
        assert!(!fully_generic_for_reflection(args(&[statik.into()]), []), "`'static`");
        assert!(
            !fully_generic_for_reflection(args(&[a.into(), b.into()]), [outlives(a, b)]),
            "a where clause on a lifetime"
        );
        assert!(
            !fully_generic_for_reflection(args(&[a.into()]), [outlives(a, statik)]),
            "a where clause naming `'static`"
        );
    }

    #[test]
    fn a_param_env_cache_call_within_another_gets_a_cache_of_its_own() {
        let key = ParamEnv::empty();
        let entry = CanonicalParamEnvCacheEntry {
            param_env: key,
            variables: Default::default(),
            variable_lookup_table: Default::default(),
            var_kinds: Vec::new(),
        };
        with_param_env_cache(|cache: &mut ParamEnvCache<'static>| {
            cache.0.insert(key, entry);
            with_param_env_cache(|inner: &mut ParamEnvCache<'static>| {
                assert!(inner.0.is_empty(), "the outer call holds the thread's cache");
            });
        });
        assert!(
            with_param_env_cache(|cache: &mut ParamEnvCache<'static>| cache.0.contains_key(&key)),
            "the outer call's entry is kept"
        );
    }

    #[test]
    fn a_fully_monomorphized_typing_env_has_no_clauses() {
        let env =
            <TypingEnv<'static> as inherent::TypingEnv<DbInterner<'static>>>::fully_monomorphized();
        assert_eq!(env.param_env, ParamEnv::empty());
    }
}
