//! The negative-fact indexes (docs/v2/items.md section 6.2): the impls of a trait by simplified
//! self type, and what a name is in a scope, each answering its question both ways ("no impl",
//! "nothing by that name" are answers too).
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

use std::collections::BTreeMap;

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
    ImplIndexKey, InterfaceHash, ItemId, ItemKey, NameHash, ScopeIndexKey, SimplifiedType,
    hash::{self, Unhashable},
    key::{self, Resolved},
};

const IMPLS_DOMAIN: &str = "blaze v2 index impls 1";
const SCOPE_DOMAIN: &str = "blaze v2 index scope 1";
const BUILTIN_DERIVE_DOMAIN: &str = "blaze v2 builtin derive interface 1";

/// The impls of `key.trait_` whose simplified self type is `key.self_ty`, by key and
/// interface hash, sorted; empty is "no impl". Answers `Implements` both ways.
pub fn impls(db: &dyn HirDatabase, key: ImplIndexKey) -> Result<ImplEntry, Unhashable> {
    match resolve_trait(db, key.trait_) {
        Some(trait_) => entry(db, impls_of(db, trait_, key.self_ty).iter().copied()),
        None => Ok(ImplEntry::TraitAbsent),
    }
}

/// The impls of `trait_` whose self type simplifies to nothing (`impl<T> Trait for T`): they
/// unify with any self type.
pub fn blanket_impls(db: &dyn HirDatabase, trait_: ItemKey) -> Result<ImplEntry, Unhashable> {
    match resolve_trait(db, trait_) {
        Some(trait_) => entry(db, blanket_impls_of(db, trait_).iter().copied()),
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
    let names = scope_names(db, owner, ());
    Ok(match names.binary_search_by_key(&key.name, |(name, _)| *name) {
        Ok(at) => names[at].1.clone(),
        Err(_) => ScopeEntry::Nothing,
    })
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

pub(crate) fn write_vis(hasher: &mut Hasher, vis: Vis) {
    match vis {
        Vis::Public => hasher.part(b"pub"),
        Vis::Crate(krate) => hasher.part(b"pub(crate)").id(&krate),
        Vis::Module(module) => hasher.part(b"pub(in)").id(&module.0),
    };
}

pub(crate) fn vis(db: &dyn HirDatabase, visibility: Visibility) -> Vis {
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

impl AnyImpl {
    pub(crate) fn key(self, db: &dyn HirDatabase) -> ItemKey {
        match self {
            AnyImpl::Impl(it) => key::item_key(db, ItemId::Impl(it)),
            AnyImpl::BuiltinDerive(it) => key::builtin_derive_key(db, it),
        }
    }

    pub(crate) fn krate(self, db: &dyn HirDatabase) -> Crate {
        match self {
            AnyImpl::Impl(it) => it.krate(db),
            AnyImpl::BuiltinDerive(it) => it.loc(db).adt.krate(db),
        }
    }
}

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
    let impls = entry(db, impls_of(db, trait_, self_ty).iter().copied())?;
    let blanket = entry(db, blanket_impls_of(db, trait_).iter().copied())?;
    Ok(Hasher::new(domain).part(b"present").id(&impls.value()).id(&blanket.value()).finish())
}

pub(crate) fn resolve_trait(db: &dyn HirDatabase, trait_: ItemKey) -> Option<TraitId> {
    match key::resolve(db, trait_)? {
        Resolved::Item(ItemId::Trait(it)) => Some(it),
        _ => unreachable!("{trait_:?} names something that is not a trait"),
    }
}

/// The hash of an impl's simplified self type, as hir-ty finds it: `None` for a blanket impl.
pub(crate) fn impl_self_ty(db: &dyn HirDatabase, imp: ImplId) -> Option<SimplifiedType> {
    let self_ty = db.impl_self_ty(imp).instantiate_identity().skip_norm_wip();
    let simplified =
        simplify_type(DbInterner::new_no_crate(db), self_ty, TreatParams::InstantiateWithInfer)?;
    Some(key::simplified(db, &simplified))
}

/// One impl of a trait in a grouping.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Grouped {
    /// `None` for a blanket impl.
    pub(crate) self_ty: Option<SimplifiedType>,
    pub(crate) key: ItemKey,
    pub(crate) imp: AnyImpl,
}

/// The impls of `trait_` in `krate`, sorted by key.
#[salsa::tracked(returns(deref))]
fn trait_impls_in(db: &dyn HirDatabase, krate: Crate, trait_: TraitId) -> Box<[Grouped]> {
    let mut found = Vec::new();
    TraitImpls::for_crate(db, krate).for_trait(trait_, |impls| match impls {
        Either::Left(impls) => found.extend(impls.iter().map(|&it| Grouped {
            self_ty: impl_self_ty(db, it),
            key: key::item_key(db, ItemId::Impl(it)),
            imp: AnyImpl::Impl(it),
        })),
        Either::Right(derives) => found.extend(derives.iter().map(|&it| {
            let adt = HirSimplifiedType::Adt(it.loc(db).adt.into());
            Grouped {
                self_ty: Some(key::simplified(db, &adt)),
                key: key::builtin_derive_key(db, it),
                imp: AnyImpl::BuiltinDerive(it),
            }
        })),
    });
    found.sort_unstable_by_key(|it| it.key);
    found.into()
}

/// The impls of `trait_` in every crate, sorted by key.
#[salsa::tracked(returns(deref))]
pub(crate) fn trait_impls(db: &dyn HirDatabase, trait_: TraitId) -> Box<[Grouped]> {
    let mut found: Vec<Grouped> = base_db::all_crates(db)
        .iter()
        .flat_map(|&krate| trait_impls_in(db, krate, trait_).iter().copied())
        .collect();
    found.sort_unstable_by_key(|it| it.key);
    found.into()
}

/// The impls of `trait_` whose simplified self type is `self_ty`, sorted by key.
#[salsa::tracked(returns(deref))]
pub(crate) fn impls_of(
    db: &dyn HirDatabase,
    trait_: TraitId,
    self_ty: SimplifiedType,
) -> Box<[(ItemKey, AnyImpl)]> {
    trait_impls(db, trait_)
        .iter()
        .filter(|it| it.self_ty == Some(self_ty))
        .map(|it| (it.key, it.imp))
        .collect()
}

/// The blanket impls of `trait_`, sorted by key.
#[salsa::tracked(returns(deref))]
pub(crate) fn blanket_impls_of(db: &dyn HirDatabase, trait_: TraitId) -> Box<[(ItemKey, AnyImpl)]> {
    trait_impls(db, trait_)
        .iter()
        .filter(|it| it.self_ty.is_none())
        .map(|it| (it.key, it.imp))
        .collect()
}

/// The inherent impls of `krate`, by the hash of their simplified self type, then key.
#[salsa::tracked(returns(deref))]
fn inherent_impls_in(db: &dyn HirDatabase, krate: Crate) -> Box<[(SimplifiedType, ItemKey, ImplId)]> {
    let mut found: Vec<_> = InherentImpls::for_crate(db, krate)
        .iter()
        .flat_map(|(self_ty, impls)| {
            let self_ty = key::simplified(db, self_ty);
            impls.iter().map(move |&it| (self_ty, key::item_key(db, ItemId::Impl(it)), it))
        })
        .collect();
    found.sort_unstable_by_key(|&(self_ty, key, _)| (self_ty, key));
    found.into()
}

/// The inherent impls of every crate for `self_ty`, sorted by key.
#[salsa::tracked(returns(deref))]
pub(crate) fn inherent_impls_of(
    db: &dyn HirDatabase,
    self_ty: SimplifiedType,
    _: (),
) -> Box<[(ItemKey, ImplId)]> {
    let mut found: Vec<_> = base_db::all_crates(db)
        .iter()
        .flat_map(|&krate| {
            let impls = inherent_impls_in(db, krate);
            let start = impls.partition_point(|&(it, _, _)| it < self_ty);
            impls[start..]
                .iter()
                .take_while(|&&(it, _, _)| it == self_ty)
                .map(|&(_, key, imp)| (key, imp))
                .collect::<Vec<_>>()
        })
        .collect();
    found.sort_unstable_by_key(|&(key, _)| key);
    found.into()
}

/// The traits declared in `krate`'s modules, by the hash of each name one of their items
/// has, then key.
#[salsa::tracked(returns(deref))]
fn traits_in(db: &dyn HirDatabase, krate: Crate) -> Box<[(NameHash, ItemKey, TraitId)]> {
    let mut found = Vec::new();
    for (_, module) in crate_def_map(db, krate).modules(db) {
        for def in module.scope(db).declarations() {
            let ModuleDefId::TraitId(trait_) = def else { continue };
            let key = key::item_key(db, ItemId::Trait(trait_));
            found.extend(
                trait_
                    .trait_items(db)
                    .items
                    .iter()
                    .map(|(name, _)| (key::name_hash(name.as_str()), key, trait_)),
            );
        }
    }
    found.sort_unstable_by_key(|&(name, key, _)| (name, key));
    found.dedup_by_key(|&mut (name, key, _)| (name, key));
    found.into()
}

/// The traits of every crate with an item whose name hashes to `name`, sorted by key.
#[salsa::tracked(returns(deref))]
pub(crate) fn traits_with(db: &dyn HirDatabase, name: NameHash, _: ()) -> Box<[(ItemKey, TraitId)]> {
    let mut found: Vec<_> = base_db::all_crates(db)
        .iter()
        .flat_map(|&krate| {
            let traits = traits_in(db, krate);
            let start = traits.partition_point(|&(it, _, _)| it < name);
            traits[start..]
                .iter()
                .take_while(|&&(it, _, _)| it == name)
                .map(|&(_, key, trait_)| (key, trait_))
                .collect::<Vec<_>>()
        })
        .collect();
    found.sort_unstable_by_key(|&(key, _)| key);
    found.into()
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

/// Every name of `owner` with what it is there, sorted by the name's hash.
#[salsa::tracked(returns(deref))]
fn scope_names(db: &dyn HirDatabase, owner: ScopeOwner, _: ()) -> Box<[(NameHash, ScopeEntry)]> {
    let mut found: Vec<(NameHash, ScopeEntry)> = match owner {
        ScopeOwner::Module(module) => module_names(db, module),
        ScopeOwner::Enum(it) => it
            .enum_variants(db)
            .variants
            .iter()
            .enumerate()
            .map(|(position, (name, &(variant, _)))| {
                let entry = ScopeEntry::Variant {
                    variant: key::variant_key(db, variant),
                    position: position as u32,
                };
                (key::name_hash(name.as_str()), entry)
            })
            .collect(),
        ScopeOwner::Variant(variant) => {
            let visibilities = VariantFields::field_visibilities(db, variant);
            variant
                .fields(db)
                .fields()
                .iter()
                .map(|(field, data)| {
                    let visibility = *visibilities.get(field).expect("every field has a visibility");
                    let entry = ScopeEntry::Field {
                        position: u32::from(field.into_raw()),
                        visibility: vis(db, visibility),
                    };
                    (key::name_hash(data.name.as_str()), entry)
                })
                .collect()
        }
        ScopeOwner::Trait(it) => assoc_names(db, &it.trait_items(db).items),
        ScopeOwner::Impl(it) => assoc_names(db, &it.impl_items(db).items),
    };
    found.sort_unstable_by_key(|(name, _)| *name);
    found.into()
}

fn assoc_names(
    db: &dyn HirDatabase,
    items: &[(hir_expand::name::Name, AssocItemId)],
) -> Vec<(NameHash, ScopeEntry)> {
    let mut by_name: BTreeMap<NameHash, Vec<(ItemKey, Vis)>> = BTreeMap::new();
    for (name, item) in items {
        let key = key::item_key(db, ItemId::from(*item));
        by_name
            .entry(key::name_hash(name.as_str()))
            .or_default()
            .push((key, vis(db, item.assoc_visibility(db))));
    }
    by_name
        .into_iter()
        .map(|(name, mut items)| {
            items.sort_unstable_by_key(|&(key, _)| key);
            (name, ScopeEntry::Assoc(items.into()))
        })
        .collect()
}

#[derive(Default)]
struct ModuleName {
    types: Option<Binding>,
    values: Option<Binding>,
    macros: Option<Binding>,
    legacy_macros: Vec<ItemKey>,
}

fn module_names(db: &dyn HirDatabase, module: ModuleId) -> Vec<(NameHash, ScopeEntry)> {
    let scope = module.def_map(db).module(db, module).scope(db);
    let mut by_name: BTreeMap<NameHash, ModuleName> = BTreeMap::new();
    let through_ns = |import: Option<ImportOrExternCrate>| match import {
        None => Through::Declared,
        Some(ImportOrExternCrate::Import(_)) => Through::Import,
        Some(ImportOrExternCrate::Glob(_)) => Through::Glob,
        Some(ImportOrExternCrate::ExternCrate(_)) => Through::ExternCrate,
    };
    for (name, per_ns) in scope.entries() {
        if per_ns.is_none() {
            continue;
        }
        let at = by_name.entry(key::name_hash(name.as_str())).or_default();
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
    for (name, macros) in scope.legacy_macros() {
        let at = by_name.entry(key::name_hash(name.as_str())).or_default();
        at.legacy_macros = macros.iter().map(|&it| key::item_key(db, ItemId::Macro(it))).collect();
    }
    by_name
        .into_iter()
        .map(|(name, it)| {
            let entry = ScopeEntry::Module {
                types: it.types,
                values: it.values,
                macros: it.macros,
                legacy_macros: it.legacy_macros.into(),
            };
            (name, entry)
        })
        .collect()
}

pub(crate) fn target(db: &dyn HirDatabase, def: ModuleDefId) -> Target {
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
