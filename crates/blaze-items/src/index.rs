//! The negative-fact indexes (docs/v2/plan/lanes/items.md section 6.2): the impls of a trait by
//! simplified self type, and what a name is in a scope, each answering its question both ways
//! ("no impl", "nothing by that name" are answers too).
//!
//! Each is built from salsa queries in layers, so an edit reruns only the groupings it touches
//! and every layer above backdates where its answer stayed the same:
//!
//! - impls: one crate's impls of one trait ([`trait_impls_in`], which reruns when the crate's
//!   impls change and backdates for every other trait), then every crate's impls of the trait
//!   ([`trait_impls`]), then the impls for one simplified self type ([`impls_of`]) and the
//!   blanket ones ([`blanket_impls_of`]);
//! - inherent impls: one crate's, by simplified self type ([`inherent_impls_in`]), then every
//!   crate's for one self type ([`inherent_impls_of`]);
//! - traits with an item of a name, for method candidates: one crate's ([`traits_in`]), then
//!   every crate's for one name ([`traits_with`]);
//! - names: one scope's, all of them ([`scope_names`]), from which [`scope`] picks one.
//!
//! Impls are found as hir-ty's `TraitImpls` and `InherentImpls` find them, in every crate:
//! the crate's modules, the bodies that may hold impls, and built-in derive impls. Traits are
//! found in every crate's modules; a trait declared in a body is visible only in that body,
//! whose own edit reruns whatever read it.
//!
//! Each index is columns (docs/v2/plan/design/data.md): the searched column apart, so a binary
//! search touches only it, keys and the cold payloads beside it, and lists as runs of one
//! flat array ([`Runs`], [`ScopeNames`]).

use std::collections::HashMap;

use base_db::Crate;
use blaze_ids::{ContentId, Hasher};
use either::Either;
use hir_def::{
    AdtId, AssocItemId, BuiltinDeriveImplId, EnumId, HasModule, ImplId, ModuleDefId, ModuleId,
    TraitId, VariantId,
    builtin_type::BuiltinType,
    item_scope::{ImportOrExternCrate, ImportOrGlob},
    nameres::crate_def_map,
    signatures::VariantFields,
    visibility::Visibility,
};
use hir_ty::{
    db::HirDatabase,
    method_resolution::{InherentImpls, TraitImpls},
    next_solver::{DbInterner, SimplifiedType as HirSimplifiedType},
};
use rustc_type_ir::fast_reject::{TreatParams, simplify_type};

use crate::{
    ImplIndexKey, InterfaceHash, ItemId, ItemKey, NAME_DOMAIN, NameHash, ScopeIndexKey,
    SimplifiedType,
    hash::{self, Unhashable},
    key::{self, KeyHashing, Resolved},
};

const IMPLS_DOMAIN: &str = "blaze v2 index impls 1";
const SCOPE_DOMAIN: &str = "blaze v2 index scope 1";
const BUILTIN_DERIVE_DOMAIN: &str = "blaze v2 builtin derive interface 1";

/// The hash of each of `names`, in order, as [`key::name_hash`] gives it, in one batched call.
pub(crate) fn name_hashes<'a>(names: impl IntoIterator<Item = &'a str>) -> Vec<NameHash> {
    let inputs: Vec<&[u8]> = names.into_iter().map(str::as_bytes).collect();
    ContentId::of_many(NAME_DOMAIN, &inputs).into_iter().map(NameHash).collect()
}

/// The impls of `key.trait_` whose simplified self type is `key.self_ty`, by key and
/// interface hash, sorted; empty is "no impl". Answers `Implements` both ways.
pub fn impls(db: &dyn HirDatabase, key: ImplIndexKey) -> Result<ImplEntry, Unhashable> {
    match resolve_trait(db, key.trait_) {
        Some(trait_) => entry(db, impls_of(db, trait_, key.self_ty).iter()),
        None => Ok(ImplEntry::TraitAbsent),
    }
}

/// The impls of `trait_` whose self type simplifies to nothing (`impl<T> Trait for T`): they
/// unify with any self type.
pub fn blanket_impls(db: &dyn HirDatabase, trait_: ItemKey) -> Result<ImplEntry, Unhashable> {
    match resolve_trait(db, trait_) {
        Some(trait_) => entry(db, blanket_impls_of(db, trait_).iter()),
        None => Ok(ImplEntry::TraitAbsent),
    }
}

/// What `key.name` is in `key.scope`; "nothing" is an answer. Answers `Resolves` both ways.
pub fn scope(db: &dyn HirDatabase, key: ScopeIndexKey) -> Result<ScopeEntry, Unhashable> {
    let Some(resolved) = key::resolve(db, key.scope) else { return Ok(ScopeEntry::ScopeAbsent) };
    let owner = match resolved {
        Resolved::Module(it) => ScopeOwner::Module(it),
        Resolved::Item(ItemId::Adt(AdtId::EnumId(it))) => ScopeOwner::Enum(it),
        Resolved::Item(ItemId::Adt(AdtId::StructId(it))) => ScopeOwner::Variant(it.into()),
        Resolved::Item(ItemId::Adt(AdtId::UnionId(it))) => ScopeOwner::Variant(it.into()),
        Resolved::Variant(it) => ScopeOwner::Variant(it.into()),
        Resolved::Item(ItemId::Trait(it)) => ScopeOwner::Trait(it),
        Resolved::Item(ItemId::Impl(it)) => ScopeOwner::Impl(it),
        Resolved::Item(_)
        | Resolved::Closure { .. }
        | Resolved::Opaque { .. }
        | Resolved::BuiltinDerive(_) => {
            unreachable!("{key:?} names something that is not a scope")
        }
    };
    Ok(scope_names(db, owner, ()).get(key.name))
}

/// The impls an index holds for one question, by key with each one's interface hash.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ImplEntry {
    /// The trait the question names no longer exists.
    TraitAbsent,
    /// Sorted by key; empty is "no impl".
    Impls(Box<[(ItemKey, InterfaceHash)]>),
}

impl ImplEntry {
    pub fn value(&self) -> ContentId {
        let mut hasher = Hasher::new(IMPLS_DOMAIN);
        match self {
            ImplEntry::TraitAbsent => {
                hasher.part(b"absent");
            }
            ImplEntry::Impls(impls) => {
                hasher.part(b"impls").update(&(impls.len() as u64).to_le_bytes());
                for (key, interface) in impls.iter() {
                    hasher.id(&key.0).id(&interface.0);
                }
            }
        }
        hasher.finish()
    }
}

/// What a name is in a scope.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum ScopeEntry {
    /// The scope the question names no longer exists.
    ScopeAbsent,
    /// Nothing by that name.
    Nothing,
    /// In a module or block, in each namespace, and the `macro_rules!` of that name in textual
    /// scope at its end, in the order they are defined (the last one shadows the others).
    Module {
        types: Option<Binding>,
        values: Option<Binding>,
        macros: Option<Binding>,
        legacy_macros: Box<[ItemKey]>,
    },
    /// In an enum: its variant of that name, at its position.
    Variant { variant: ItemKey, position: u32 },
    /// In a struct, union or variant: its field of that name.
    Field { position: u32, visibility: Vis },
    /// In a trait or an impl: its associated items of that name (a type and a value may share
    /// one), sorted by key.
    Assoc(Box<[(ItemKey, Vis)]>),
}

/// What a name means in one namespace of a module.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Binding {
    pub target: Target,
    pub visibility: Vis,
    pub through: Through,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    Item(ItemKey),
    /// A primitive type (`u8`, `str`), by its name.
    Builtin(NameHash),
}

/// How a name came to be in a module: an explicit item or import shadows a glob.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Through {
    Declared,
    Import,
    Glob,
    ExternCrate,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Vis {
    Public,
    /// Visible in the crate of this key.
    Crate(ContentId),
    /// Visible in this module and the ones in it.
    Module(ItemKey),
}

const _: () = assert!(size_of::<Vis>() == 33);
const _: () = assert!(size_of::<Binding>() == 67);
const _: () = assert!(size_of::<Option<Binding>>() == 67);

impl ScopeEntry {
    pub fn value(&self) -> ContentId {
        let mut hasher = Hasher::new(SCOPE_DOMAIN);
        match self {
            ScopeEntry::ScopeAbsent => {
                hasher.part(b"absent");
            }
            ScopeEntry::Nothing => {
                hasher.part(b"nothing");
            }
            ScopeEntry::Module { types, values, macros, legacy_macros } => {
                hasher.part(b"module");
                for binding in [types, values, macros] {
                    write_binding(&mut hasher, binding.as_ref());
                }
                hasher.update(&(legacy_macros.len() as u64).to_le_bytes());
                for it in legacy_macros.iter() {
                    hasher.id(&it.0);
                }
            }
            ScopeEntry::Variant { variant, position } => {
                hasher.part(b"variant").id(&variant.0).update(&position.to_le_bytes());
            }
            ScopeEntry::Field { position, visibility } => {
                hasher.part(b"field").update(&position.to_le_bytes());
                write_vis(&mut hasher, *visibility);
            }
            ScopeEntry::Assoc(items) => {
                hasher.part(b"assoc").update(&(items.len() as u64).to_le_bytes());
                for &(key, visibility) in items.iter() {
                    hasher.id(&key.0);
                    write_vis(&mut hasher, visibility);
                }
            }
        }
        hasher.finish()
    }
}

fn write_binding(hasher: &mut Hasher, binding: Option<&Binding>) {
    let Some(binding) = binding else {
        hasher.part(b"none");
        return;
    };
    match binding.target {
        Target::Item(key) => hasher.part(b"item").id(&key.0),
        Target::Builtin(name) => hasher.part(b"builtin").id(&name.0),
    };
    write_vis(hasher, binding.visibility);
    hasher.part(match binding.through {
        Through::Declared => b"declared".as_slice(),
        Through::Import => b"import",
        Through::Glob => b"glob",
        Through::ExternCrate => b"extern crate",
    });
}

fn write_vis(hasher: &mut Hasher, vis: Vis) {
    match vis {
        Vis::Public => hasher.part(b"pub"),
        Vis::Crate(krate) => hasher.part(b"pub(crate)").id(&krate),
        Vis::Module(module) => hasher.part(b"pub(in)").id(&module.0),
    };
}

fn vis(db: &dyn HirDatabase, visibility: Visibility) -> Vis {
    match visibility {
        Visibility::Public => Vis::Public,
        Visibility::PubCrate(krate) => Vis::Crate(key::crate_key(db, krate)),
        Visibility::Module(module, _) => Vis::Module(key::module_key(db, module)),
    }
}

/// An impl as the indexes hold it.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum AnyImpl {
    Impl(ImplId),
    BuiltinDerive(BuiltinDeriveImplId),
}

const _: () = assert!(size_of::<AnyImpl>() == 8);

/// An impl's interface hash. A built-in derive impl has no item of its own: it is what the
/// derive writes for its type, so it is its key (which names the trait) and the type's
/// interface.
pub(crate) fn impl_interface(
    db: &dyn HirDatabase,
    imp: AnyImpl,
) -> Result<InterfaceHash, Unhashable> {
    match imp {
        AnyImpl::Impl(it) => hash::interface_hash(db, ItemId::Impl(it)),
        AnyImpl::BuiltinDerive(it) => {
            let adt = hash::interface_hash(db, ItemId::Adt(it.loc(db).adt))?;
            let key = key::builtin_derive_key(db, it);
            Ok(InterfaceHash(Hasher::new(BUILTIN_DERIVE_DOMAIN).id(&key.0).id(&adt.0).finish()))
        }
    }
}

/// `impls` with their interface hashes, in the order given.
pub(crate) fn entry(
    db: &dyn HirDatabase,
    impls: impl IntoIterator<Item = (ItemKey, AnyImpl)>,
) -> Result<ImplEntry, Unhashable> {
    let impls = impls
        .into_iter()
        .map(|(key, imp)| Ok((key, impl_interface(db, imp)?)))
        .collect::<Result<Box<[_]>, Unhashable>>()?;
    Ok(ImplEntry::Impls(impls))
}

/// The value of `Implements { trait_, self_ty }`: the impls for that self type and the
/// blanket ones.
pub(crate) fn implements_value(
    db: &dyn HirDatabase,
    domain: &'static str,
    trait_: TraitId,
    self_ty: SimplifiedType,
) -> Result<ContentId, Unhashable> {
    let (impls, blanket) = implements_entries(db, trait_, self_ty)?;
    Ok(Hasher::new(domain).part(b"present").id(&impls).id(&blanket).finish())
}

/// The values of the impls of `trait_` for `self_ty` and of its blanket impls, hashed once
/// for every fact that reads them.
#[salsa::tracked]
fn implements_entries(
    db: &dyn HirDatabase,
    trait_: TraitId,
    self_ty: SimplifiedType,
) -> Result<(ContentId, ContentId), Unhashable> {
    let impls = entry(db, impls_of(db, trait_, self_ty).iter())?;
    let blanket = entry(db, blanket_impls_of(db, trait_).iter())?;
    Ok((impls.value(), blanket.value()))
}

/// The value of every impl of `trait_`, in the order of [`trait_impls`]: what a blanket impl's
/// coherence reads, hashed once per trait rather than once per blanket impl.
#[salsa::tracked]
pub(crate) fn all_impls_value(
    db: &dyn HirDatabase,
    trait_: TraitId,
) -> Result<ContentId, Unhashable> {
    Ok(entry(db, trait_impls(db, trait_).all().iter())?.value())
}

pub(crate) fn resolve_trait(db: &dyn HirDatabase, trait_: ItemKey) -> Option<TraitId> {
    match key::resolve(db, trait_)? {
        Resolved::Item(ItemId::Trait(it)) => Some(it),
        _ => unreachable!("{trait_:?} names something that is not a trait"),
    }
}

/// The hash of an impl's simplified self type, as hir-ty's `TraitImpls` and `InherentImpls`
/// find it: `None` for a blanket impl.
pub(crate) fn impl_self_ty(db: &dyn HirDatabase, imp: ImplId) -> Option<SimplifiedType> {
    let self_ty = match db.impl_trait(imp) {
        Some(trait_ref) => trait_ref.instantiate_identity().skip_norm_wip().self_ty(),
        None => db.impl_self_ty(imp).instantiate_identity().skip_norm_wip(),
    };
    let simplified =
        simplify_type(DbInterner::new_no_crate(db), self_ty, TreatParams::InstantiateWithInfer)?;
    Some(key::simplified(db, &simplified))
}

/// Rows sorted by a searched column, then key, stored as columns. `searched` holds each
/// distinct searched value once, sorted, and is all a lookup's binary search touches.
/// `starts[i]..starts[i + 1]` is the run of `searched[i]` in `keys` and `values`, and the
/// rows before `starts[0]` have no searched value (a trait's blanket impls). `values` is
/// read only after the search.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Runs<S, T> {
    searched: Box<[S]>,
    starts: Box<[u32]>,
    keys: Box<[ItemKey]>,
    values: Box<[T]>,
}

impl<S: Copy + Ord, T: Copy> Runs<S, T> {
    fn new(mut rows: Vec<(Option<S>, ItemKey, T)>) -> Self {
        rows.sort_unstable_by_key(|&(at, key, _)| (at, key));
        let mut searched = Vec::new();
        let mut starts = Vec::new();
        let mut keys = Vec::with_capacity(rows.len());
        let mut values = Vec::with_capacity(rows.len());
        for (row, (at, key, value)) in rows.into_iter().enumerate() {
            if let Some(at) = at {
                if searched.last() != Some(&at) {
                    searched.push(at);
                    starts.push(row_index(row));
                }
            }
            keys.push(key);
            values.push(value);
        }
        starts.push(row_index(keys.len()));
        Runs {
            searched: searched.into(),
            starts: starts.into(),
            keys: keys.into(),
            values: values.into(),
        }
    }

    /// The rows whose searched value is `at`, sorted by key; `None` is the rows with none.
    fn run(&self, at: Option<S>) -> Columns<'_, T> {
        let rows = match at {
            None => 0..self.starts[0] as usize,
            Some(at) => match self.searched.binary_search(&at) {
                Ok(i) => self.starts[i] as usize..self.starts[i + 1] as usize,
                Err(_) => 0..0,
            },
        };
        Columns { keys: &self.keys[rows.clone()], values: &self.values[rows] }
    }

    /// Every row, in order.
    pub(crate) fn all(&self) -> Columns<'_, T> {
        Columns { keys: &self.keys, values: &self.values }
    }

    /// Every row with its searched value, in order.
    fn rows(&self) -> impl Iterator<Item = (Option<S>, ItemKey, T)> + '_ {
        let none = self.run(None).iter().map(|(key, value)| (None, key, value));
        let runs = self.searched.iter().enumerate().flat_map(move |(i, &at)| {
            let rows = self.starts[i] as usize..self.starts[i + 1] as usize;
            Columns { keys: &self.keys[rows.clone()], values: &self.values[rows] }
                .iter()
                .map(move |(key, value)| (Some(at), key, value))
        });
        none.chain(runs)
    }
}

fn row_index(row: usize) -> u32 {
    u32::try_from(row).expect("an index has fewer than 2^32 rows")
}

/// Rows as two columns of one length: keys, and what each key is.
#[derive(Clone, Copy)]
pub(crate) struct Columns<'a, T> {
    keys: &'a [ItemKey],
    values: &'a [T],
}

impl<'a, T: Copy> Columns<'a, T> {
    pub(crate) fn iter(self) -> impl Iterator<Item = (ItemKey, T)> + 'a {
        self.keys.iter().copied().zip(self.values.iter().copied())
    }
}

/// One question's rows, sorted by key, as two columns.
#[derive(Clone, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Keyed<T> {
    keys: Box<[ItemKey]>,
    values: Box<[T]>,
}

impl<T: Copy> Keyed<T> {
    fn sorted(mut rows: Vec<(ItemKey, T)>) -> Self {
        rows.sort_unstable_by_key(|&(key, _)| key);
        Keyed::of(rows)
    }

    fn of(rows: impl IntoIterator<Item = (ItemKey, T)>) -> Self {
        let (keys, values): (Vec<_>, Vec<_>) = rows.into_iter().unzip();
        Keyed { keys: keys.into(), values: values.into() }
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = (ItemKey, T)> + '_ {
        Columns { keys: &self.keys, values: &self.values }.iter()
    }
}

/// The impls of `trait_` in `krate`, as [`trait_impls`] holds them.
#[salsa::tracked(returns(ref))]
fn trait_impls_in(
    db: &dyn HirDatabase,
    krate: Crate,
    trait_: TraitId,
) -> Runs<SimplifiedType, AnyImpl> {
    let mut found = Vec::new();
    TraitImpls::for_crate(db, krate).for_trait(trait_, |impls| match impls {
        Either::Left(impls) => found.extend(impls.iter().map(|&it| {
            (impl_self_ty(db, it), key::item_key(db, ItemId::Impl(it)), AnyImpl::Impl(it))
        })),
        Either::Right(derives) => found.extend(derives.iter().map(|&it| {
            let adt = HirSimplifiedType::Adt(it.loc(db).adt.into());
            (
                Some(key::simplified(db, &adt)),
                key::builtin_derive_key(db, it),
                AnyImpl::BuiltinDerive(it),
            )
        })),
    });
    Runs::new(found)
}

/// The impls of `trait_` in every crate, by simplified self type (the blanket ones as their
/// own run, first), then key.
#[salsa::tracked(returns(ref))]
pub(crate) fn trait_impls(db: &dyn HirDatabase, trait_: TraitId) -> Runs<SimplifiedType, AnyImpl> {
    Runs::new(
        dependents(db, trait_.krate(db))
            .iter()
            .flat_map(|&krate| trait_impls_in(db, krate, trait_).rows())
            .collect(),
    )
}

/// `krate` and the crates that depend on it, directly or not: the only ones that can hold an
/// impl of one of its traits, which the impl names.
#[salsa::tracked(returns(deref))]
fn dependents(db: &dyn HirDatabase, krate: Crate) -> Box<[Crate]> {
    krate.transitive_rev_deps(db)
}

/// The impls of `trait_` whose simplified self type is `self_ty`, sorted by key.
#[salsa::tracked(returns(ref))]
pub(crate) fn impls_of(
    db: &dyn HirDatabase,
    trait_: TraitId,
    self_ty: SimplifiedType,
) -> Keyed<AnyImpl> {
    Keyed::of(trait_impls(db, trait_).run(Some(self_ty)).iter())
}

/// The blanket impls of `trait_`, sorted by key.
#[salsa::tracked(returns(ref))]
pub(crate) fn blanket_impls_of(db: &dyn HirDatabase, trait_: TraitId) -> Keyed<AnyImpl> {
    Keyed::of(trait_impls(db, trait_).run(None).iter())
}

/// The inherent impls of `krate`, by the hash of their simplified self type, then key.
#[salsa::tracked(returns(ref))]
fn inherent_impls_in(db: &dyn HirDatabase, krate: Crate) -> Runs<SimplifiedType, ImplId> {
    Runs::new(
        InherentImpls::for_crate(db, krate)
            .iter()
            .flat_map(|(self_ty, impls)| {
                let self_ty = key::simplified(db, self_ty);
                impls
                    .iter()
                    .map(move |&it| (Some(self_ty), key::item_key(db, ItemId::Impl(it)), it))
            })
            .collect(),
    )
}

/// The inherent impls of every crate for `self_ty`, sorted by key.
#[salsa::tracked(returns(ref))]
pub(crate) fn inherent_impls_of(
    db: &dyn HirDatabase,
    self_ty: SimplifiedType,
    _: (),
) -> Keyed<ImplId> {
    Keyed::sorted(
        base_db::all_crates(db)
            .iter()
            .flat_map(|&krate| inherent_impls_in(db, krate).run(Some(self_ty)).iter())
            .collect(),
    )
}

/// The traits declared in `krate`'s modules, by the hash of each name one of their items
/// has, then key.
#[salsa::tracked(returns(ref))]
fn traits_in(db: &dyn HirDatabase, krate: Crate) -> Runs<NameHash, TraitId> {
    let mut names = Vec::new();
    let mut traits = Vec::new();
    for (_, module) in crate_def_map(db, krate).modules(db) {
        for def in module.scope(db).declarations() {
            let ModuleDefId::TraitId(trait_) = def else { continue };
            let key = key::item_key(db, ItemId::Trait(trait_));
            for (name, _) in trait_.trait_items(db).items.iter() {
                names.push(name.as_str());
                traits.push((key, trait_));
            }
        }
    }
    let mut found: Vec<_> = name_hashes(names)
        .into_iter()
        .zip(traits)
        .map(|(name, (key, trait_))| (Some(name), key, trait_))
        .collect();
    found.sort_unstable_by_key(|&(name, key, _)| (name, key));
    found.dedup_by_key(|&mut (name, key, _)| (name, key));
    Runs::new(found)
}

/// The traits of every crate with an item whose name hashes to `name`, sorted by key.
#[salsa::tracked(returns(ref))]
pub(crate) fn traits_with(db: &dyn HirDatabase, name: NameHash, _: ()) -> Keyed<TraitId> {
    Keyed::sorted(
        base_db::all_crates(db)
            .iter()
            .flat_map(|&krate| traits_in(db, krate).run(Some(name)).iter())
            .collect(),
    )
}

/// A scope names are looked up in.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum ScopeOwner {
    Module(ModuleId),
    Enum(EnumId),
    Variant(VariantId),
    Trait(TraitId),
    Impl(ImplId),
}

/// One scope's names as columns: `names`, sorted, is all a lookup's binary search touches;
/// what the name at a position is sits at that position in `entries`.
#[derive(Clone, PartialEq, Eq, Debug)]
pub(crate) struct ScopeNames {
    names: Box<[NameHash]>,
    entries: ScopeColumns,
}

/// What each name of a scope is, one column per part (a scope's names are all of one kind).
/// `starts[i]..starts[i + 1]` is name `i`'s run in the flat columns after it.
#[derive(Clone, PartialEq, Eq, Debug)]
enum ScopeColumns {
    Module {
        types: Box<[Option<Binding>]>,
        values: Box<[Option<Binding>]>,
        macros: Box<[Option<Binding>]>,
        starts: Box<[u32]>,
        legacy_macros: Box<[ItemKey]>,
    },
    Variant {
        variants: Box<[ItemKey]>,
        positions: Box<[u32]>,
    },
    Field {
        positions: Box<[u32]>,
        visibilities: Box<[Vis]>,
    },
    Assoc {
        starts: Box<[u32]>,
        keys: Box<[ItemKey]>,
        visibilities: Box<[Vis]>,
    },
}

impl ScopeNames {
    fn get(&self, name: NameHash) -> ScopeEntry {
        let Ok(at) = self.names.binary_search(&name) else { return ScopeEntry::Nothing };
        let run = |starts: &[u32]| starts[at] as usize..starts[at + 1] as usize;
        match &self.entries {
            ScopeColumns::Module { types, values, macros, starts, legacy_macros } => {
                ScopeEntry::Module {
                    types: types[at],
                    values: values[at],
                    macros: macros[at],
                    legacy_macros: legacy_macros[run(starts)].into(),
                }
            }
            ScopeColumns::Variant { variants, positions } => {
                ScopeEntry::Variant { variant: variants[at], position: positions[at] }
            }
            ScopeColumns::Field { positions, visibilities } => {
                ScopeEntry::Field { position: positions[at], visibility: visibilities[at] }
            }
            ScopeColumns::Assoc { starts, keys, visibilities } => {
                let items = run(starts);
                let keys = keys[items.clone()].iter().copied();
                ScopeEntry::Assoc(keys.zip(visibilities[items].iter().copied()).collect())
            }
        }
    }
}

/// Every name of `owner` with what it is there, sorted by the name's hash.
#[salsa::tracked(returns(ref))]
fn scope_names(db: &dyn HirDatabase, owner: ScopeOwner, _: ()) -> ScopeNames {
    match owner {
        ScopeOwner::Module(module) => module_names(db, module),
        ScopeOwner::Enum(it) => {
            let variants = &it.enum_variants(db).variants;
            let mut rows: Vec<_> = name_hashes(variants.keys().map(|name| name.as_str()))
                .into_iter()
                .zip(variants.values())
                .enumerate()
                .map(|(position, (name, &(variant, _)))| {
                    (name, key::variant_key(db, variant), position as u32)
                })
                .collect();
            rows.sort_unstable_by_key(|&(name, _, _)| name);
            ScopeNames {
                names: rows.iter().map(|it| it.0).collect(),
                entries: ScopeColumns::Variant {
                    variants: rows.iter().map(|it| it.1).collect(),
                    positions: rows.iter().map(|it| it.2).collect(),
                },
            }
        }
        ScopeOwner::Variant(variant) => {
            let visibilities = VariantFields::field_visibilities(db, variant);
            let fields = variant.fields(db).fields();
            let mut rows: Vec<_> = name_hashes(fields.iter().map(|(_, data)| data.name.as_str()))
                .into_iter()
                .zip(fields.iter())
                .map(|(name, (field, _))| {
                    let visibility = visibilities.get(field).expect("every field has a visibility");
                    (name, u32::from(field.into_raw()), vis(db, *visibility))
                })
                .collect();
            rows.sort_unstable_by_key(|&(name, position, _)| (name, position));
            ScopeNames {
                names: rows.iter().map(|it| it.0).collect(),
                entries: ScopeColumns::Field {
                    positions: rows.iter().map(|it| it.1).collect(),
                    visibilities: rows.iter().map(|it| it.2).collect(),
                },
            }
        }
        ScopeOwner::Trait(it) => assoc_names(db, &it.trait_items(db).items),
        ScopeOwner::Impl(it) => assoc_names(db, &it.impl_items(db).items),
    }
}

/// A trait's or impl's items by name, each name's run sorted by key (a type and a value may
/// share a name).
fn assoc_names(
    db: &dyn HirDatabase,
    items: &[(hir_expand::name::Name, AssocItemId)],
) -> ScopeNames {
    let mut rows: Vec<_> = name_hashes(items.iter().map(|(name, _)| name.as_str()))
        .into_iter()
        .zip(items)
        .map(|(name, &(_, item))| {
            (name, key::item_key(db, ItemId::from(item)), vis(db, item.assoc_visibility(db)))
        })
        .collect();
    rows.sort_unstable_by_key(|&(name, key, _)| (name, key));
    let mut names = Vec::new();
    let mut starts = Vec::new();
    for (row, &(name, _, _)) in rows.iter().enumerate() {
        if names.last() != Some(&name) {
            names.push(name);
            starts.push(row_index(row));
        }
    }
    starts.push(row_index(rows.len()));
    ScopeNames {
        names: names.into(),
        entries: ScopeColumns::Assoc {
            starts: starts.into(),
            keys: rows.iter().map(|it| it.1).collect(),
            visibilities: rows.iter().map(|it| it.2).collect(),
        },
    }
}

/// One name of a module while its columns are built; `legacy` is its position in the
/// module's legacy macros.
#[derive(Default)]
struct ModuleName {
    types: Option<Binding>,
    values: Option<Binding>,
    macros: Option<Binding>,
    legacy: Option<usize>,
}

fn module_names(db: &dyn HirDatabase, module: ModuleId) -> ScopeNames {
    let scope = module.def_map(db).module(db, module).scope(db);
    let entries: Vec<_> = scope.entries().filter(|(_, per_ns)| !per_ns.is_none()).collect();
    let legacy: Vec<_> = scope.legacy_macros().collect();
    let names = name_hashes(
        entries
            .iter()
            .map(|(name, _)| name.as_str())
            .chain(legacy.iter().map(|(name, _)| name.as_str())),
    );
    let (entry_names, legacy_names) = names.split_at(entries.len());
    let mut by_name: HashMap<NameHash, ModuleName, KeyHashing> =
        HashMap::with_capacity_and_hasher(names.len(), KeyHashing);
    let through_ns = |import: Option<ImportOrExternCrate>| match import {
        None => Through::Declared,
        Some(ImportOrExternCrate::Import(_)) => Through::Import,
        Some(ImportOrExternCrate::Glob(_)) => Through::Glob,
        Some(ImportOrExternCrate::ExternCrate(_)) => Through::ExternCrate,
    };
    for (&name, (_, per_ns)) in entry_names.iter().zip(entries) {
        let at = by_name.entry(name).or_default();
        at.types = per_ns.types.map(|it| Binding {
            target: target(db, it.def),
            visibility: vis(db, it.vis),
            through: through_ns(it.import),
        });
        at.values = per_ns.values.map(|it| Binding {
            target: target(db, it.def),
            visibility: vis(db, it.vis),
            through: match it.import {
                None => Through::Declared,
                Some(ImportOrGlob::Import(_)) => Through::Import,
                Some(ImportOrGlob::Glob(_)) => Through::Glob,
            },
        });
        at.macros = per_ns.macros.map(|it| Binding {
            target: Target::Item(key::item_key(db, ItemId::Macro(it.def))),
            visibility: vis(db, it.vis),
            through: through_ns(it.import),
        });
    }
    for (position, &name) in legacy_names.iter().enumerate() {
        by_name.entry(name).or_default().legacy = Some(position);
    }
    let mut rows: Vec<_> = by_name.into_iter().collect();
    rows.sort_unstable_by_key(|&(name, _)| name);
    let mut starts = Vec::with_capacity(rows.len() + 1);
    let mut legacy_macros = Vec::new();
    for (_, it) in &rows {
        starts.push(row_index(legacy_macros.len()));
        if let Some(position) = it.legacy {
            let (_, macros) = legacy[position];
            legacy_macros.extend(macros.iter().map(|&it| key::item_key(db, ItemId::Macro(it))));
        }
    }
    starts.push(row_index(legacy_macros.len()));
    ScopeNames {
        names: rows.iter().map(|&(name, _)| name).collect(),
        entries: ScopeColumns::Module {
            types: rows.iter().map(|(_, it)| it.types).collect(),
            values: rows.iter().map(|(_, it)| it.values).collect(),
            macros: rows.iter().map(|(_, it)| it.macros).collect(),
            starts: starts.into(),
            legacy_macros: legacy_macros.into(),
        },
    }
}

fn target(db: &dyn HirDatabase, def: ModuleDefId) -> Target {
    let item = |id| Target::Item(key::item_key(db, id));
    match def {
        ModuleDefId::ModuleId(it) => Target::Item(key::module_key(db, it)),
        ModuleDefId::FunctionId(it) => item(ItemId::Function(it)),
        ModuleDefId::AdtId(it) => item(ItemId::Adt(it)),
        ModuleDefId::EnumVariantId(it) => Target::Item(key::variant_key(db, it)),
        ModuleDefId::ConstId(it) => item(ItemId::Const(it)),
        ModuleDefId::StaticId(it) => item(ItemId::Static(it)),
        ModuleDefId::TraitId(it) => item(ItemId::Trait(it)),
        ModuleDefId::TypeAliasId(it) => item(ItemId::Alias(it)),
        ModuleDefId::MacroId(it) => item(ItemId::Macro(it)),
        ModuleDefId::BuiltinType(it) => {
            let (name, _) = BuiltinType::all_builtin_types()
                .into_iter()
                .find(|&(_, ty)| ty == it)
                .expect("every builtin type has a name");
            Target::Builtin(key::name_hash(name.as_str()))
        }
    }
}
