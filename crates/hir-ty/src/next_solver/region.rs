//! Things related to regions.

use std::{marker::PhantomData, num::NonZeroU32};

use hir_def::LifetimeParamId;
use macros::GenericTypeVisitable;
use rustc_type_ir::{
    BoundVarIndexKind, DebruijnIndex, Flags, GenericTypeVisitable, INNERMOST, RegionVid, TypeFlags,
    TypeFoldable, TypeVisitable,
    inherent::{IntoKind, SliceLike},
    relate::Relate,
};

use crate::next_solver::{
    GenericArg, OutlivesPredicate, impl_foldable_for_interned_slice, impl_stored_interned,
    interned_slice, interner::table, persist,
};

use super::{SolverDefId, interner::DbInterner};

pub type RegionKind<'db> = rustc_type_ir::RegionKind<DbInterner<'db>>;
pub type RegionConstraint<'db> = rustc_type_ir::RegionConstraint<DbInterner<'db>>;

/// Every interned [`Region`]. A `GenericArg` holds a row in 30 bits.
static REGIONS: table::Rows<RegionKind<'static>> = table::Rows::new("regions", table::LAST_ROW);
table::erase!(RegionKind);

/// A region: its row in [`REGIONS`], which is never freed.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct Region<'db> {
    index: NonZeroU32,
    _db: PhantomData<&'db ()>,
}

const _: () = assert!(size_of::<Option<Region<'static>>>() == 4);
// A row costs `table::ROW_BYTES` hot bytes and its kind, at most 48 cold ones.
const _: () = assert!(size_of::<RegionKind<'static>>() <= 48);

impl_stored_interned!(Region, StoredRegion);

impl<'db> Region<'db> {
    /// You should avoid using this if you can, since we want `Region` to be defined in `rustc_type_ir` and then this method
    /// will become more difficult to use.
    pub fn new_without_interner(kind: RegionKind<'db>) -> Self {
        Self::intern(kind)
    }

    pub fn new(_interner: DbInterner<'db>, kind: RegionKind<'db>) -> Self {
        Self::intern(kind)
    }

    #[inline]
    pub(super) fn intern(kind: RegionKind<'db>) -> Self {
        Self::at(REGIONS.intern(kind, || table::RowInfo {
            tag: persist::tag(&kind),
            flags: region_flags(kind),
            binder: region_binder(kind),
        }))
    }

    /// This region's row: the same number for the same region, on every thread, for the life
    /// of the process.
    #[inline]
    pub fn index(self) -> u32 {
        self.index.get()
    }

    /// The region at `index`, which [`Self::index`] gave. Panics on any other number.
    pub fn from_index(_interner: DbInterner<'db>, index: u32) -> Self {
        match NonZeroU32::new(index) {
            Some(index) if REGIONS.published(index) => Self::at(index),
            _ => panic!("{index} is not the index of an interned region"),
        }
    }

    #[inline]
    pub(super) fn at(index: NonZeroU32) -> Self {
        Region { index, _db: PhantomData }
    }

    #[inline]
    pub(super) fn row(self) -> NonZeroU32 {
        self.index
    }

    pub fn new_early_param(
        interner: DbInterner<'db>,
        early_bound_region: EarlyParamRegion,
    ) -> Self {
        Region::new(interner, RegionKind::ReEarlyParam(early_bound_region))
    }

    pub fn new_placeholder(interner: DbInterner<'db>, placeholder: PlaceholderRegion<'db>) -> Self {
        Region::new(interner, RegionKind::RePlaceholder(placeholder))
    }

    pub fn new_var(interner: DbInterner<'db>, v: RegionVid) -> Region<'db> {
        Region::new(interner, RegionKind::ReVar(v))
    }

    pub fn new_erased(interner: DbInterner<'db>) -> Region<'db> {
        interner.default_types().regions.erased
    }

    pub fn new_bound(
        interner: DbInterner<'db>,
        index: DebruijnIndex,
        bound: BoundRegion<'db>,
    ) -> Region<'db> {
        Region::new(interner, RegionKind::ReBound(BoundVarIndexKind::Bound(index), bound))
    }

    pub fn new_late_param(
        interner: DbInterner<'db>,
        scope: SolverDefId<'db>,
        bound_region: BoundRegion<'db>,
    ) -> Region<'db> {
        let late_bound_region = LateParamRegion { scope, bound_region };
        Region::new(interner, RegionKind::ReLateParam(late_bound_region))
    }

    pub fn is_placeholder(&self) -> bool {
        matches!(self.kind(), RegionKind::RePlaceholder(..))
    }

    pub fn is_static(&self) -> bool {
        matches!(self.kind(), RegionKind::ReStatic)
    }

    pub fn is_erased(&self) -> bool {
        matches!(self.kind(), RegionKind::ReErased)
    }

    pub fn is_var(&self) -> bool {
        matches!(self.kind(), RegionKind::ReVar(_))
    }

    pub fn is_error(&self) -> bool {
        matches!(self.kind(), RegionKind::ReError(_))
    }

    pub fn error(interner: DbInterner<'db>) -> Self {
        interner.default_types().regions.error
    }

    #[inline]
    pub fn type_flags(&self) -> TypeFlags {
        REGIONS.flags(self.index)
    }
}

/// The flags of a region of `kind`, as `rustc_middle` computes them.
fn region_flags(kind: RegionKind<'_>) -> TypeFlags {
    let mut flags = TypeFlags::empty();
    match kind {
        RegionKind::ReVar(..) => {
            flags |= TypeFlags::HAS_FREE_REGIONS;
            flags |= TypeFlags::HAS_FREE_LOCAL_REGIONS;
            flags |= TypeFlags::HAS_RE_INFER;
        }
        RegionKind::RePlaceholder(..) => {
            flags |= TypeFlags::HAS_FREE_REGIONS;
            flags |= TypeFlags::HAS_FREE_LOCAL_REGIONS;
            flags |= TypeFlags::HAS_RE_PLACEHOLDER;
        }
        RegionKind::ReEarlyParam(..) => {
            flags |= TypeFlags::HAS_FREE_REGIONS;
            flags |= TypeFlags::HAS_FREE_LOCAL_REGIONS;
            flags |= TypeFlags::HAS_RE_PARAM;
        }
        RegionKind::ReLateParam(..) => {
            flags |= TypeFlags::HAS_FREE_REGIONS;
            flags |= TypeFlags::HAS_FREE_LOCAL_REGIONS;
        }
        RegionKind::ReStatic => {
            flags |= TypeFlags::HAS_FREE_REGIONS;
        }
        RegionKind::ReBound(BoundVarIndexKind::Canonical, ..) => {
            flags |= TypeFlags::HAS_RE_BOUND;
            flags |= TypeFlags::HAS_CANONICAL_BOUND;
        }
        RegionKind::ReBound(BoundVarIndexKind::Bound(..), ..) => {
            flags |= TypeFlags::HAS_RE_BOUND;
        }
        RegionKind::ReErased => {
            flags |= TypeFlags::HAS_RE_ERASED;
        }
        RegionKind::ReError(..) => {
            flags |= TypeFlags::HAS_FREE_REGIONS;
            flags |= TypeFlags::HAS_RE_ERROR;
        }
    }
    flags
}

fn region_binder(kind: RegionKind<'_>) -> DebruijnIndex {
    match kind {
        RegionKind::ReBound(BoundVarIndexKind::Bound(debruijn), _) => debruijn.shifted_in(1),
        _ => INNERMOST,
    }
}

pub type PlaceholderRegion<'db> = rustc_type_ir::PlaceholderRegion<DbInterner<'db>>;

#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct EarlyParamRegion {
    // FIXME: See `ParamTy`.
    pub id: LifetimeParamId,
    pub index: u32,
}

#[derive(Copy, Clone, PartialEq, Eq, Hash, GenericTypeVisitable)]
/// Represents a liberated late-bound function lifetime parameter.
///
/// This denotes some region at least as big as `scope`. It is similar to a placeholder region
/// created when entering a binder, except it always lives in the root universe.
pub struct LateParamRegion<'db> {
    pub scope: SolverDefId<'db>,
    pub bound_region: BoundRegion<'db>,
}

impl std::fmt::Debug for LateParamRegion<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ReLateParam({:?}, {:?})", self.scope, self.bound_region)
    }
}

pub type BoundRegion<'db> = rustc_type_ir::BoundRegion<DbInterner<'db>>;
pub type BoundRegionKind<'db> = rustc_type_ir::BoundRegionKind<DbInterner<'db>>;

impl rustc_type_ir::inherent::ParamLike for EarlyParamRegion {
    fn index(self) -> u32 {
        self.index
    }
}

impl std::fmt::Debug for EarlyParamRegion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "#{}", self.index)
        // write!(f, "{}/#{}", self.name, self.index)
    }
}

impl std::fmt::Debug for Region<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.kind().fmt(f)
    }
}

impl<'db> IntoKind for Region<'db> {
    type Kind = RegionKind<'db>;

    #[inline]
    fn kind(self) -> Self::Kind {
        REGIONS.kind(self.index)
    }
}

impl<'db> TypeVisitable<DbInterner<'db>> for Region<'db> {
    fn visit_with<V: rustc_type_ir::TypeVisitor<DbInterner<'db>>>(
        &self,
        visitor: &mut V,
    ) -> V::Result {
        visitor.visit_region(*self)
    }
}

impl<'db> TypeFoldable<DbInterner<'db>> for Region<'db> {
    fn try_fold_with<F: rustc_type_ir::FallibleTypeFolder<DbInterner<'db>>>(
        self,
        folder: &mut F,
    ) -> Result<Self, F::Error> {
        folder.try_fold_region(self)
    }
    fn fold_with<F: rustc_type_ir::TypeFolder<DbInterner<'db>>>(self, folder: &mut F) -> Self {
        folder.fold_region(self)
    }
}

impl<'db> Relate<DbInterner<'db>> for Region<'db> {
    fn relate<R: rustc_type_ir::relate::TypeRelation<DbInterner<'db>>>(
        relation: &mut R,
        a: Self,
        b: Self,
    ) -> rustc_type_ir::relate::RelateResult<DbInterner<'db>, Self> {
        relation.regions(a, b)
    }
}

impl<'db> Flags for Region<'db> {
    fn flags(&self) -> rustc_type_ir::TypeFlags {
        self.type_flags()
    }

    #[inline]
    fn outer_exclusive_binder(&self) -> rustc_type_ir::DebruijnIndex {
        REGIONS.binder(self.index)
    }
}

impl<'db> rustc_type_ir::inherent::Region<DbInterner<'db>> for Region<'db> {
    fn new_bound(
        interner: DbInterner<'db>,
        debruijn: DebruijnIndex,
        var: BoundRegion<'db>,
    ) -> Self {
        Region::new(interner, RegionKind::ReBound(BoundVarIndexKind::Bound(debruijn), var))
    }

    fn new_anon_bound(
        interner: DbInterner<'db>,
        debruijn: DebruijnIndex,
        var: rustc_type_ir::BoundVar,
    ) -> Self {
        Region::new(
            interner,
            RegionKind::ReBound(
                BoundVarIndexKind::Bound(debruijn),
                BoundRegion { var, kind: BoundRegionKind::Anon },
            ),
        )
    }

    fn new_canonical_bound(interner: DbInterner<'db>, var: rustc_type_ir::BoundVar) -> Self {
        Region::new(
            interner,
            RegionKind::ReBound(
                BoundVarIndexKind::Canonical,
                BoundRegion { var, kind: BoundRegionKind::Anon },
            ),
        )
    }

    fn new_static(interner: DbInterner<'db>) -> Self {
        interner.default_types().regions.statik
    }

    fn new_placeholder(interner: DbInterner<'db>, var: PlaceholderRegion<'db>) -> Self {
        Region::new(interner, RegionKind::RePlaceholder(var))
    }
}

impl<'db, V: super::WorldExposer> GenericTypeVisitable<V> for Region<'db> {
    fn generic_visit_with(&self, visitor: &mut V) {
        if visitor.on_interned(REGIONS.name(), self.index.get()).is_continue() {
            self.kind().generic_visit_with(visitor);
        }
    }
}

type GenericArgOutlivesPredicate<'db> = OutlivesPredicate<'db, GenericArg<'db>>;

interned_slice!(
    RegionAssumptionsStorage,
    RegionAssumptions,
    StoredRegionAssumptions,
    region_assumptions,
    GenericArgOutlivesPredicate<'db>,
    GenericArgOutlivesPredicate<'static>,
);
impl_foldable_for_interned_slice!(RegionAssumptions);
