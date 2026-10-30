//! Signatures lowered a crate and a kind at a time (docs/v2/plan/design/data.md, "batch by
//! kind"): the field types of every variant, the self type, trait and predicates of every impl.
//! A batch is columns: the items in ascending order, their values in a column beside them (the
//! field types as one CSR list), and what lowering found (diagnostics, anonymous constants) in a
//! sparse side table, since nearly every row has none.
//!
//! Only the items of a crate's modules are in its batch. An item declared in a block is in a
//! body, and a signature query reads no body, so such items keep a query of their own.

use base_db::Crate;
use hir_def::{AdtId, ImplId, ModuleDefId, ModuleId, VariantId, nameres::crate_def_map};
use rustc_type_ir::inherent::Ty as _;
use salsa::SalsaValue;
use thin_vec::ThinVec;

use crate::{
    TyLoweringDiagnostic,
    db::{AnonConstId, HirDatabase},
    lower::{
        FieldType, FieldTypes, GenericPredicates, Lowered, TyLoweringResult, generic_predicates,
        lower_field_types, lower_impl_self_ty, lower_impl_trait,
    },
    next_solver::{
        Clauses, DbInterner, ErrorGuaranteed, StoredEarlyBinder, StoredTraitRef, StoredTy, Ty,
    },
};

/// The crate whose batch holds the signatures of the items of `module`, `None` when they have a
/// query each.
///
/// A row of a batch is read through the batch, so its reader depends on every item of that kind
/// in the crate: an edit of one struct's fields recomputes the field types of all the crate's
/// variants (work linear in the crate, where a query per item recomputes one), and every query
/// that read a row of it runs again to find its row unchanged. A dependency crate (origin not
/// `Local`: a library, a lang crate, a rustc crate) is not edited by the user, so its batch is
/// computed once and kept, which costs nothing on an edit and saves a memo, its edges and its
/// allocations per item. A workspace crate is edited, so its items keep a query each. An edited
/// path dependency is a library too: it recomputes its batch, a crate's worth of signatures, on
/// each edit of one of them.
pub(crate) fn batched_crate(db: &dyn HirDatabase, module: ModuleId) -> Option<Crate> {
    if module.block(db).is_some() {
        return None;
    }
    let krate = module.krate(db);
    (!krate.data(db).origin.is_local()).then_some(krate)
}

/// The diagnostics and anonymous constants of the rows that have any, ascending by row.
#[derive(Debug, Default, PartialEq, Eq, SalsaValue)]
pub struct SideTable<'db> {
    pub(super) rows: Box<[u32]>,
    /// Where each of `rows` starts in `diagnostics`; it ends where the next starts.
    pub(super) diagnostics_at: Box<[u32]>,
    /// Where each of `rows` starts in `anon_consts`; it ends where the next starts.
    pub(super) anon_consts_at: Box<[u32]>,
    pub(super) diagnostics: Box<[TyLoweringDiagnostic]>,
    pub(super) anon_consts: Box<[AnonConstId<'db>]>,
}

const _: () = assert!(size_of::<SideTable<'static>>() == 5 * 16);
const _: () = assert!(size_of::<CrateImplSelfTys<'static>>() == 2 * 16 + size_of::<SideTable<'static>>());
const _: () = assert!(size_of::<CrateFieldTypes<'static>>() == 3 * 16 + size_of::<SideTable<'static>>());

impl<'db> SideTable<'db> {
    /// The diagnostics and anonymous constants of `row`.
    pub(crate) fn get(&self, row: u32) -> (&[TyLoweringDiagnostic], &[AnonConstId<'db>]) {
        let Ok(at) = self.rows.binary_search(&row) else { return (&[], &[]) };
        let range = |starts: &[u32], len: usize| {
            let end = starts.get(at + 1).map_or(len, |&it| it as usize);
            starts[at] as usize..end
        };
        (
            &self.diagnostics[range(&self.diagnostics_at, self.diagnostics.len())],
            &self.anon_consts[range(&self.anon_consts_at, self.anon_consts.len())],
        )
    }

    pub(crate) fn len(&self) -> usize {
        self.rows.len()
    }
}

/// Builds a [`SideTable`], rows pushed in ascending order.
#[derive(Default)]
pub(crate) struct SideTableBuilder<'db> {
    rows: Vec<u32>,
    diagnostics_at: Vec<u32>,
    anon_consts_at: Vec<u32>,
    diagnostics: Vec<TyLoweringDiagnostic>,
    anon_consts: Vec<AnonConstId<'db>>,
}

impl<'db> SideTableBuilder<'db> {
    pub(crate) fn push(
        &mut self,
        row: u32,
        diagnostics: impl IntoIterator<Item = TyLoweringDiagnostic>,
        anon_consts: impl IntoIterator<Item = AnonConstId<'db>>,
    ) {
        debug_assert!(self.rows.last().is_none_or(|&last| last < row), "rows out of order");
        let (diagnostics_at, anon_consts_at) =
            (self.diagnostics.len() as u32, self.anon_consts.len() as u32);
        self.diagnostics.extend(diagnostics);
        self.anon_consts.extend(anon_consts);
        if self.diagnostics.len() as u32 == diagnostics_at
            && self.anon_consts.len() as u32 == anon_consts_at
        {
            return;
        }
        self.rows.push(row);
        self.diagnostics_at.push(diagnostics_at);
        self.anon_consts_at.push(anon_consts_at);
    }

    pub(crate) fn finish(self) -> SideTable<'db> {
        SideTable {
            rows: self.rows.into_boxed_slice(),
            diagnostics_at: self.diagnostics_at.into_boxed_slice(),
            anon_consts_at: self.anon_consts_at.into_boxed_slice(),
            diagnostics: self.diagnostics.into_boxed_slice(),
            anon_consts: self.anon_consts.into_boxed_slice(),
        }
    }
}

/// One kind of signature of a crate's items: the items ascending, a value for each.
#[derive(Debug, PartialEq, Eq, SalsaValue)]
pub struct Batch<'db, K, V> {
    pub(super) keys: Box<[K]>,
    pub(super) values: Box<[V]>,
    pub(super) side: SideTable<'db>,
}

impl<'db, K: Ord + Copy, V> Batch<'db, K, V> {
    /// `lower` of each of `keys`, which are ascending and distinct.
    fn build(keys: Vec<K>, mut lower: impl FnMut(K) -> TyLoweringResult<'db, V>) -> Self {
        let mut values = Vec::with_capacity(keys.len());
        let mut side = SideTableBuilder::default();
        for (row, &key) in keys.iter().enumerate() {
            let (value, diagnostics, anon_consts) = lower(key).into_parts();
            side.push(row as u32, diagnostics, anon_consts);
            values.push(value);
        }
        Batch { keys: keys.into_boxed_slice(), values: values.into_boxed_slice(), side: side.finish() }
    }

    /// `value()` for each of `keys`: the batch when computing it reached itself, which only code
    /// rustc rejects does (an impl header naming its own self type or trait). Each row is what
    /// the per-item query's cycle result gives.
    fn filled(keys: Vec<K>, value: impl Fn() -> V) -> Self {
        let values = keys.iter().map(|_| value()).collect();
        Batch { keys: keys.into_boxed_slice(), values, side: SideTable::default() }
    }

    pub(crate) fn len(&self) -> usize {
        self.keys.len()
    }

    fn row(&self, key: K) -> Option<usize> {
        self.keys.binary_search(&key).ok()
    }

    fn lowered(&'db self, row: usize) -> Lowered<'db, V> {
        let (diagnostics, anon_consts) = self.side.get(row as u32);
        Lowered::new(&self.values[row], diagnostics, anon_consts)
    }
}

/// The self type of each impl of a crate's modules.
pub type CrateImplSelfTys<'db> = Batch<'db, ImplId, StoredEarlyBinder<StoredTy>>;

/// The trait of each impl of a crate's modules, `None` for an inherent impl.
pub type CrateImplTraits<'db> = Batch<'db, ImplId, Option<StoredEarlyBinder<StoredTraitRef>>>;

/// The predicates of each impl of a crate's modules, which coherence and method resolution read
/// for all of a crate's impls together.
pub type CrateImplPredicates<'db> = Batch<'db, ImplId, GenericPredicates>;

impl<'db> CrateImplSelfTys<'db> {
    pub fn of(db: &'db dyn HirDatabase, krate: Crate) -> &'db Self {
        crate_impl_self_tys(db, krate)
    }

    /// `None` for an impl not of this crate's modules.
    pub fn get(&'db self, impl_id: ImplId) -> Option<Lowered<'db, StoredEarlyBinder<StoredTy>>> {
        Some(self.lowered(self.row(impl_id)?))
    }
}

impl<'db> CrateImplTraits<'db> {
    pub fn of(db: &'db dyn HirDatabase, krate: Crate) -> &'db Self {
        crate_impl_traits(db, krate)
    }

    /// `None` for an impl not of this crate's modules, `Some(None)` for an inherent impl.
    #[allow(clippy::option_option)]
    pub fn get(
        &'db self,
        impl_id: ImplId,
    ) -> Option<Option<Lowered<'db, StoredEarlyBinder<StoredTraitRef>>>> {
        let row = self.row(impl_id)?;
        let (diagnostics, anon_consts) = self.side.get(row as u32);
        Some(self.values[row].as_ref().map(|it| Lowered::new(it, diagnostics, anon_consts)))
    }
}

impl<'db> CrateImplPredicates<'db> {
    pub fn of(db: &'db dyn HirDatabase, krate: Crate) -> &'db Self {
        crate_impl_predicates(db, krate)
    }

    /// `None` for an impl not of this crate's modules.
    pub fn get(&'db self, impl_id: ImplId) -> Option<Lowered<'db, GenericPredicates>> {
        Some(self.lowered(self.row(impl_id)?))
    }
}

/// The field types of every variant of a crate's modules: the structs, the unions and each
/// variant of the enums, ascending, their fields one CSR list.
#[derive(Debug, PartialEq, Eq, SalsaValue)]
pub struct CrateFieldTypes<'db> {
    pub(super) variants: Box<[VariantId]>,
    /// The fields of `variants[i]` are `fields[starts[i]..starts[i + 1]]`: one more than
    /// `variants`.
    pub(super) starts: Box<[u32]>,
    pub(super) fields: Box<[FieldType]>,
    pub(super) side: SideTable<'db>,
}

impl<'db> CrateFieldTypes<'db> {
    pub fn of(db: &'db dyn HirDatabase, krate: Crate) -> &'db Self {
        crate_field_types(db, krate)
    }

    /// `None` for a variant not of this crate's modules.
    pub fn get(&'db self, variant: VariantId) -> Option<Lowered<'db, FieldTypes>> {
        let row = self.variants.binary_search(&variant).ok()?;
        let fields = &self.fields[self.starts[row] as usize..self.starts[row + 1] as usize];
        let (diagnostics, anon_consts) = self.side.get(row as u32);
        Some(Lowered::new(FieldTypes::from_slice(fields), diagnostics, anon_consts))
    }

    pub fn len(&self) -> usize {
        self.variants.len()
    }

    pub fn is_empty(&self) -> bool {
        self.variants.is_empty()
    }

    /// The batch of `lower` of each of `variants`, which are ascending and distinct.
    pub(crate) fn build(
        variants: Vec<VariantId>,
        mut lower: impl FnMut(VariantId) -> TyLoweringResult<'db, ThinVec<FieldType>>,
    ) -> Self {
        let mut starts = Vec::with_capacity(variants.len() + 1);
        let mut fields = Vec::new();
        let mut side = SideTableBuilder::default();
        starts.push(0);
        for (row, &variant) in variants.iter().enumerate() {
            let (types, diagnostics, anon_consts) = lower(variant).into_parts();
            fields.extend(types);
            starts.push(u32::try_from(fields.len()).expect("more than 2^32 fields in one crate"));
            side.push(row as u32, diagnostics, anon_consts);
        }
        CrateFieldTypes {
            variants: variants.into_boxed_slice(),
            starts: starts.into_boxed_slice(),
            fields: fields.into_boxed_slice(),
            side: side.finish(),
        }
    }
}

/// The variants of a crate's modules, ascending.
fn module_variants(db: &dyn HirDatabase, krate: Crate) -> Vec<VariantId> {
    let mut variants = Vec::new();
    for (_, module) in crate_def_map(db, krate).modules(db) {
        for def in module.scope(db).declarations() {
            match def {
                ModuleDefId::AdtId(AdtId::StructId(it)) => variants.push(VariantId::from(it)),
                ModuleDefId::AdtId(AdtId::UnionId(it)) => variants.push(VariantId::from(it)),
                ModuleDefId::AdtId(AdtId::EnumId(it)) => variants.extend(
                    it.enum_variants(db).variants.values().map(|&(it, _)| VariantId::from(it)),
                ),
                _ => {}
            }
        }
    }
    variants.sort_unstable();
    variants.dedup();
    variants
}

/// The impls of a crate's modules, ascending.
fn module_impls(db: &dyn HirDatabase, krate: Crate) -> Vec<ImplId> {
    let mut impls = Vec::new();
    for (_, module) in crate_def_map(db, krate).modules(db) {
        impls.extend(module.scope(db).impls());
    }
    impls.sort_unstable();
    impls.dedup();
    impls
}

#[salsa::tracked(returns(ref))]
fn crate_field_types<'db>(db: &'db dyn HirDatabase, krate: Crate) -> CrateFieldTypes<'db> {
    CrateFieldTypes::build(module_variants(db, krate), |variant| lower_field_types(db, variant))
}

#[salsa::tracked(returns(ref), cycle_result = crate_impl_self_tys_cycle)]
fn crate_impl_self_tys<'db>(db: &'db dyn HirDatabase, krate: Crate) -> CrateImplSelfTys<'db> {
    Batch::build(module_impls(db, krate), |impl_id| lower_impl_self_ty(db, impl_id))
}

fn crate_impl_self_tys_cycle<'db>(
    db: &'db dyn HirDatabase,
    _: salsa::Id,
    krate: Crate,
) -> CrateImplSelfTys<'db> {
    let error = Ty::new_error(DbInterner::new_no_crate(db), ErrorGuaranteed).store();
    Batch::filled(module_impls(db, krate), || StoredEarlyBinder::bind(error.clone()))
}

/// Reads the crate's self types: a trait's arguments may name `Self`.
#[salsa::tracked(returns(ref), cycle_result = crate_impl_traits_cycle)]
fn crate_impl_traits<'db>(db: &'db dyn HirDatabase, krate: Crate) -> CrateImplTraits<'db> {
    let self_tys = CrateImplSelfTys::of(db, krate);
    let keys = self_tys.keys.to_vec();
    Batch::build(keys, |impl_id| {
        let self_ty = self_tys.get(impl_id).expect("the self types have every impl");
        match lower_impl_trait(db, impl_id, self_ty.value.get().skip_binder()) {
            Some(result) => {
                let (value, diagnostics, anon_consts) = result.into_parts();
                TyLoweringResult::new(Some(value), diagnostics, anon_consts)
            }
            None => TyLoweringResult::empty(None),
        }
    })
}

fn crate_impl_traits_cycle<'db>(
    db: &'db dyn HirDatabase,
    _: salsa::Id,
    krate: Crate,
) -> CrateImplTraits<'db> {
    Batch::filled(module_impls(db, krate), || None)
}

/// Reads the crate's self types and traits: a where clause may name `Self` and `Self::Assoc`.
#[salsa::tracked(returns(ref), cycle_result = crate_impl_predicates_cycle)]
fn crate_impl_predicates<'db>(db: &'db dyn HirDatabase, krate: Crate) -> CrateImplPredicates<'db> {
    let keys = CrateImplSelfTys::of(db, krate).keys.to_vec();
    Batch::build(keys, |impl_id| generic_predicates(db, impl_id.into()))
}

fn crate_impl_predicates_cycle<'db>(
    db: &'db dyn HirDatabase,
    _: salsa::Id,
    krate: Crate,
) -> CrateImplPredicates<'db> {
    let empty = StoredEarlyBinder::bind(Clauses::empty().store());
    Batch::filled(module_impls(db, krate), || {
        GenericPredicates::from_explicit_own_predicates(empty.clone())
    })
}

#[cfg(test)]
mod tests {
    use hir_def::type_ref::TypeRefId;
    use la_arena::RawIdx;

    use super::*;
    use crate::lower::diagnostics::{GenericArgsProhibitedReason, PathLoweringDiagnostic};

    fn diagnostic(segment: u32) -> TyLoweringDiagnostic {
        TyLoweringDiagnostic::PathDiagnostic {
            source: TypeRefId::from_raw(RawIdx::from_u32(segment)),
            diag: PathLoweringDiagnostic::GenericArgsProhibited {
                segment,
                reason: GenericArgsProhibitedReason::Module,
            },
        }
    }

    #[test]
    fn a_side_table_keeps_only_rows_with_something() {
        let mut side = SideTableBuilder::<'static>::default();
        side.push(0, [], []);
        side.push(1, [diagnostic(1), diagnostic(2)], []);
        side.push(2, [], []);
        side.push(5, [diagnostic(5)], []);
        side.push(9, [], []);
        let side = side.finish();
        assert_eq!(side.len(), 2);
        assert_eq!(side.get(0), (&[][..], &[][..]));
        assert_eq!(side.get(1).0, &[diagnostic(1), diagnostic(2)]);
        assert_eq!(side.get(2).0, &[]);
        assert_eq!(side.get(5).0, &[diagnostic(5)]);
        assert_eq!(side.get(9).0, &[]);
        assert_eq!(side.get(100).0, &[]);
    }

    #[test]
    fn an_empty_side_table_allocates_nothing_and_has_nothing() {
        let side = SideTableBuilder::<'static>::default().finish();
        assert_eq!(side.len(), 0);
        assert!(side.get(0).0.is_empty() && side.get(0).1.is_empty());
    }
}
