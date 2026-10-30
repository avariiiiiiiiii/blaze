//! What each [`Fact`] stands for (docs/v2/plan/lanes/items.md section 6.2): [`value`] computes
//! it from the current database, and a query that read the fact is reused while its
//! [`fingerprint`] stays the same. A fact names its subject by key; a key that names nothing now
//! gives the value "absent", never an error, so a deleted item is a changed fact like any other.
//!
//! The rules that reach across items are each read through a fact:
//!
//! | rule | fact |
//! |---|---|
//! | coherence, orphan check | `Coherence(impl)` |
//! | specialization | `Specializes(impl)` |
//! | inference fallback | `Implements` for the fallback type: `assert_eq!(1.5, s.parse().unwrap())` reads the impls of `PartialEq` for `f64`; integer and `!` fallback are the edition's, in the crate's config |
//! | drop check | `DropCheck(T)` |
//! | vtable layout | `Vtable { trait_, self_ty }` |
//! | derives | `Derive(T)` |
//! | glob imports, shadowing | `Resolves { scope, name }` |
//! | macro hygiene | `Resolves` in the def-site scope for def-site names; mixed-site names resolve at the call site |
//! | auto-trait leakage | `AutoTraits(opaque)` |
//! | the target's data layout | `Target(crate)` |

use std::{alloc::Allocator, fmt::Write as _};

use base_db::Db;
use blaze_ids::{ContentId, Fingerprint, Hasher, radix};
use blaze_mem::{Frame, Tag, frame, scratch};
use either::Either;
use hir_def::{
    AdtId, AssocItemId, ConstId, DefWithBodyId, ExpressionStoreOwnerId, FunctionId, HasModule,
    ImplId, Lookup, ModuleId, TraitId, VariantId,
    attrs::AttrFlags,
    expr_store::{Body, ExpressionStore},
    hir::{ClosureKind, ExprId},
    item_scope::ItemScope,
    lang_item::lang_items,
    unstable_features::UnstableFeatures,
};
use hir_expand::{AstId, HirFileId, MacroCallId, name::Name};
use hir_ty::{
    InferBodyId, InferenceResult, MemoryMap, ParamEnvAndCrate, all_super_traits,
    check_orphan_rules,
    consteval::ConstEvalError,
    db::{AnonConstId, HirDatabase, InternedClosure, InternedClosureId},
    generics::generics,
    layout::{RustcEnumVariantIdx, Variants},
    next_solver::{GenericArgs, SimplifiedType as HirSimplifiedType, Ty},
    traits::is_inherent_impl_coherent,
};
use syntax::ast;

use crate::{
    Fact, InterfaceHash, ItemId, ItemKey, NameHash, ScopeIndexKey, SimplifiedType,
    hash::{self, Unhashable},
    index::{
        AnyImpl, ScopeEntry, all_impls_value, entry, impl_interface, impl_self_ty,
        implements_value, inherent_impls_of, name_hashes, resolve_trait, scope, traits_with,
    },
    key::{self, Resolved},
};

const SIGNATURE: &str = "blaze v2 fact signature 1";
const LAYOUT: &str = "blaze v2 fact layout 1";
const FIELD: &str = "blaze v2 fact field 1";
const METHOD: &str = "blaze v2 fact method 1";
const IMPLEMENTS: &str = "blaze v2 fact implements 1";
const RESOLVES: &str = "blaze v2 fact resolves 1";
const CODE: &str = "blaze v2 fact code 1";
const CONST_VALUE: &str = "blaze v2 fact const value 1";
const NAME: &str = "blaze v2 fact name 1";
const CAPTURES: &str = "blaze v2 fact captures 1";
const AUTO_TRAITS: &str = "blaze v2 fact auto traits 1";
const COHERENCE: &str = "blaze v2 fact coherence 1";
const SPECIALIZES: &str = "blaze v2 fact specializes 1";
const DROP_CHECK: &str = "blaze v2 fact drop check 1";
const VTABLE: &str = "blaze v2 fact vtable 1";
const DERIVE: &str = "blaze v2 fact derive 1";
const TARGET: &str = "blaze v2 fact target 1";
const CONST_TYPE: &str = "blaze v2 fact const value type 1";

fn domain(fact: &Fact) -> &'static str {
    match fact {
        Fact::Signature(_) => SIGNATURE,
        Fact::Layout(_) => LAYOUT,
        Fact::Field { .. } => FIELD,
        Fact::Method { .. } => METHOD,
        Fact::Implements { .. } => IMPLEMENTS,
        Fact::Resolves { .. } => RESOLVES,
        Fact::Code(_) => CODE,
        Fact::ConstValue(_) => CONST_VALUE,
        Fact::Name(_) => NAME,
        Fact::Captures(_) => CAPTURES,
        Fact::AutoTraits(_) => AUTO_TRAITS,
        Fact::Coherence(_) => COHERENCE,
        Fact::Specializes(_) => SPECIALIZES,
        Fact::DropCheck(_) => DROP_CHECK,
        Fact::Vtable { .. } => VTABLE,
        Fact::Derive(_) => DERIVE,
        Fact::Target(_) => TARGET,
    }
}

/// What `fact` stands for in the current database (see the module doc).
pub fn value(db: &Db, fact: &Fact) -> Result<ContentId, Unhashable> {
    hir_ty::attach_db(db, || value_in(db, fact))
}

fn value_in(db: &Db, fact: &Fact) -> Result<ContentId, Unhashable> {
    let domain = domain(fact);
    let absent = || Ok(Hasher::new(domain).part(b"absent").finish());
    let mut h = Hasher::new(domain);
    h.part(b"present");
    match *fact {
        Fact::Signature(key) => {
            let Some(id) = item(db, key, fact) else { return absent() };
            h.id(&hash::interface_hash(db, id)?.0);
        }
        Fact::Layout(key) => {
            let Some(adt) = adt(db, key, fact) else { return absent() };
            h.id(&hash::layout_hash(db, adt)?.0);
        }
        Fact::Field { ty, field } => {
            let variant = match key::resolve(db, ty) {
                None => return absent(),
                Some(Resolved::Item(ItemId::Adt(AdtId::StructId(it)))) => VariantId::from(it),
                Some(Resolved::Item(ItemId::Adt(AdtId::UnionId(it)))) => VariantId::from(it),
                Some(Resolved::Variant(it)) => VariantId::from(it),
                Some(other) => unreachable!("{fact:?} names {other:?}, which has no fields"),
            };
            let Some((local, _)) =
                variant.fields(db).fields().iter().find(|(it, _)| u32::from(it.into_raw()) == field)
            else {
                return absent();
            };
            let adt = variant.adt_id(db);
            let sub = crate::form::Cx::new(db).field(variant, local);
            h.id(&hash::filled(db, domain, &sub, ItemId::Adt(adt))?);
            field_offset(db, &mut h, adt, variant, local)?;
        }
        Fact::Method { self_ty, name } => write_method(db, &mut h, self_ty, name)?,
        Fact::Implements { trait_, self_ty } => {
            let Some(trait_) = resolve_trait(db, trait_) else { return absent() };
            h.id(&implements_value(db, IMPLEMENTS, trait_, self_ty)?);
        }
        Fact::Resolves { scope: at, name } => match scope(db, ScopeIndexKey { scope: at, name })? {
            ScopeEntry::ScopeAbsent => return absent(),
            found => {
                h.id(&found.value());
            }
        },
        Fact::Code(key) => {
            let Some(id) = item(db, key, fact) else { return absent() };
            write_code(&mut h, hash::code_hash(db, id)?.map(|it| it.0));
        }
        Fact::ConstValue(subject) => {
            let konst = match key::resolve(db, subject) {
                None => return absent(),
                Some(Resolved::Item(ItemId::Const(it))) => it,
                Some(other) => unreachable!("{fact:?} names {other:?}, which is no const"),
            };
            write_const_value(db, &mut h, konst)?;
        }
        Fact::Name(key) => {
            let Some(id) = item(db, key, fact) else { return absent() };
            name_of(db, &mut h, id);
        }
        Fact::Captures(subject) => {
            let (store, expr, kind) = match key::resolve(db, subject) {
                None => return absent(),
                Some(Resolved::Closure { store, expr, kind }) => (store, expr, kind),
                Some(other) => unreachable!("{fact:?} names {other:?}, which is no closure"),
            };
            let closure = match kind {
                ClosureKind::Closure | ClosureKind::CoroutineClosure(_) => {
                    let owner = closure_owner(db, store, expr);
                    InternedClosureId::new(db, InternedClosure { owner, expr, kind })
                }
                ClosureKind::OldCoroutine(_) | ClosureKind::Coroutine { .. } => {
                    unreachable!("{fact:?} names a coroutine, which has no closure captures")
                }
            };
            let sub = crate::form::Cx::new(db).captures(closure);
            h.id(&hash::filled(db, domain, &sub, infer_owner_item(db, closure.loc(db).owner))?);
        }
        Fact::AutoTraits(subject) => {
            let (owner, index) = match key::resolve(db, subject) {
                None => return absent(),
                Some(Resolved::Opaque { owner, index }) => (owner, index),
                Some(other) => unreachable!("{fact:?} names {other:?}, which is no opaque type"),
            };
            let sub = crate::form::Cx::new(db).auto_traits(owner, index);
            h.id(&hash::filled(db, domain, &sub, owner)?);
        }
        Fact::Coherence(key) => {
            let Some(imp) = impl_of(db, key, fact) else { return absent() };
            write_coherence(db, &mut h, imp)?;
        }
        Fact::Specializes(key) => {
            let Some(imp) = impl_of(db, key, fact) else { return absent() };
            h.id(&hash::interface_hash(db, ItemId::Impl(imp))?.0);
            write_coherence(db, &mut h, imp)?;
            let features = UnstableFeatures::query(db, imp.krate(db));
            h.update(&[features.specialization as u8, features.min_specialization as u8]);
        }
        Fact::DropCheck(key) => {
            let Some(adt) = adt(db, key, fact) else { return absent() };
            write_drop_check(db, &mut h, adt)?;
        }
        Fact::Vtable { trait_, self_ty } => {
            let Some(trait_) = resolve_trait(db, trait_) else { return absent() };
            h.id(&hash::interface_hash(db, ItemId::Trait(trait_))?.0);
            let supers = all_super_traits(db, trait_);
            h.update(&(supers.len() as u64).to_le_bytes());
            for &it in supers {
                h.id(&hash::interface_hash(db, ItemId::Trait(it))?.0)
                    .id(&implements_value(db, IMPLEMENTS, it, self_ty)?);
                // A vtable's slots are its methods in declaration order, which the trait's
                // interface hash leaves out.
                let items = &it.trait_items(db).items;
                let methods = || {
                    items.iter().filter_map(|(name, item)| match *item {
                        AssocItemId::FunctionId(f) => Some((name, f)),
                        _ => None,
                    })
                };
                h.update(&(methods().count() as u64).to_le_bytes());
                let names = name_hashes(methods().map(|(name, _)| name.as_str()));
                for (name, (_, f)) in names.into_iter().zip(methods()) {
                    h.id(&name.0).id(&hash::interface_hash(db, ItemId::Function(f))?.0);
                }
            }
        }
        Fact::Derive(key) => {
            let Some(adt) = adt(db, key, fact) else { return absent() };
            write_derive(db, &mut h, adt)?;
        }
        Fact::Target(krate) => {
            let Some(krate) = key::resolve_crate(db, &krate) else { return absent() };
            match &krate.workspace_data(db).target {
                Ok(target) => {
                    h.part(b"target").id(&target.content());
                }
                Err(err) => scratch::with_string(|text| {
                    write!(text, "{err}").expect("writing to a string cannot fail");
                    h.part(b"error").part(text.as_bytes());
                }),
            }
        }
    }
    Ok(h.finish())
}

/// The first 128 bits of [`value`]: what a dependency edge records.
pub fn fingerprint(db: &Db, fact: &Fact) -> Result<Fingerprint, Unhashable> {
    Ok(value(db, fact)?.fingerprint())
}

pub fn signature(db: &Db, f: FunctionId) -> Fact {
    Fact::Signature(key::item_key(db, ItemId::Function(f)))
}

pub fn layout(db: &Db, ty: AdtId) -> Fact {
    Fact::Layout(key::item_key(db, ItemId::Adt(ty)))
}

pub fn field(db: &Db, variant: VariantId, field: hir_def::LocalFieldId) -> Fact {
    let ty = match variant {
        VariantId::StructId(it) => key::item_key(db, ItemId::Adt(it.into())),
        VariantId::UnionId(it) => key::item_key(db, ItemId::Adt(it.into())),
        VariantId::EnumVariantId(it) => key::variant_key(db, it),
    };
    Fact::Field { ty, field: u32::from(field.into_raw()) }
}

pub fn method(db: &Db, self_ty: &HirSimplifiedType<'_>, name: &Name) -> Fact {
    Fact::Method { self_ty: key::simplified(db, self_ty), name: key::name_hash(name.as_str()) }
}

pub fn implements(db: &Db, trait_: TraitId, self_ty: &HirSimplifiedType<'_>) -> Fact {
    Fact::Implements {
        trait_: key::item_key(db, ItemId::Trait(trait_)),
        self_ty: key::simplified(db, self_ty),
    }
}

pub fn resolves(db: &Db, scope: ModuleId, name: &Name) -> Fact {
    Fact::Resolves { scope: key::module_key(db, scope), name: key::name_hash(name.as_str()) }
}

pub fn code(db: &Db, id: ItemId) -> Fact {
    Fact::Code(key::item_key(db, id))
}

pub fn const_value(db: &Db, konst: ConstId) -> Fact {
    Fact::ConstValue(key::item_key(db, ItemId::Const(konst)))
}

pub fn name(db: &Db, id: ItemId) -> Fact {
    Fact::Name(key::item_key(db, id))
}

pub fn captures(db: &Db, closure: InternedClosureId<'_>) -> Fact {
    Fact::Captures(key::closure_key(db, closure))
}

pub fn auto_traits(db: &Db, owner: ItemId, index: u32) -> Fact {
    Fact::AutoTraits(key::opaque_key(db, owner, index))
}

pub fn coherence(db: &Db, imp: ImplId) -> Fact {
    Fact::Coherence(key::item_key(db, ItemId::Impl(imp)))
}

pub fn specializes(db: &Db, imp: ImplId) -> Fact {
    Fact::Specializes(key::item_key(db, ItemId::Impl(imp)))
}

pub fn drop_check(db: &Db, ty: AdtId) -> Fact {
    Fact::DropCheck(key::item_key(db, ItemId::Adt(ty)))
}

pub fn vtable(db: &Db, trait_: TraitId, self_ty: &HirSimplifiedType<'_>) -> Fact {
    Fact::Vtable {
        trait_: key::item_key(db, ItemId::Trait(trait_)),
        self_ty: key::simplified(db, self_ty),
    }
}

pub fn derive(db: &Db, ty: AdtId) -> Fact {
    Fact::Derive(key::item_key(db, ItemId::Adt(ty)))
}

pub fn target(db: &Db, krate: base_db::Crate) -> Fact {
    Fact::Target(key::crate_key(db, krate))
}

/// The item `subject` names, `None` if none does.
fn item(db: &Db, subject: ItemKey, fact: &Fact) -> Option<ItemId> {
    match key::resolve(db, subject)? {
        Resolved::Item(id) => Some(id),
        other => unreachable!("{fact:?} names {other:?}, which is no item"),
    }
}

fn adt(db: &Db, key: ItemKey, fact: &Fact) -> Option<AdtId> {
    match item(db, key, fact)? {
        ItemId::Adt(it) => Some(it),
        other => unreachable!("{fact:?} names {other:?}, which is no type"),
    }
}

fn impl_of(db: &Db, key: ItemKey, fact: &Fact) -> Option<ImplId> {
    match item(db, key, fact)? {
        ItemId::Impl(it) => Some(it),
        other => unreachable!("{fact:?} names {other:?}, which is no impl"),
    }
}

fn write_code(h: &mut Hasher, code: Option<ContentId>) {
    match code {
        Some(code) => h.update(&[1]).id(&code),
        None => h.update(&[0]),
    };
}

fn write_symbol(h: &mut Hasher, symbol: Option<intern::Symbol>) {
    match symbol {
        Some(symbol) => h.update(&[1]).part(symbol.as_str().as_bytes()),
        None => h.update(&[0]),
    };
}

/// The offset of a field of a type without generics, from its layout. A generic type has a
/// layout per instance: its layout hash stands for all of them.
fn field_offset(
    db: &Db,
    h: &mut Hasher,
    adt: AdtId,
    variant: VariantId,
    field: hir_def::LocalFieldId,
) -> Result<(), Unhashable> {
    if generics(db, adt.into()).iter(true).next().is_some() {
        h.part(b"generic").id(&hash::layout_hash(db, adt)?.0);
        return Ok(());
    }
    let param_env = db.trait_environment(adt.into());
    let env = ParamEnvAndCrate { param_env, krate: adt.krate(db) };
    let layout = match db.layout_of_adt(adt, GenericArgs::empty().store(), env.store()) {
        Ok(layout) => layout,
        Err(err) => {
            scratch::with_string(|text| {
                write!(text, "{err}").expect("writing to a string cannot fail");
                h.part(b"layout error").part(text.as_bytes());
            });
            return Ok(());
        }
    };
    let at = u32::from(field.into_raw()) as usize;
    let offset = match (&layout.variants, variant) {
        (Variants::Multiple { variants, .. }, VariantId::EnumVariantId(it)) => variants
            .iter()
            .nth(it.index(db))
            .and_then(|kept| kept.field_offsets.iter().nth(at).copied()),
        (Variants::Single { index: RustcEnumVariantIdx(single) }, VariantId::EnumVariantId(it))
            if *single != it.index(db) =>
        {
            None
        }
        (Variants::Empty, _) => None,
        _ => (at < layout.fields.count()).then(|| layout.fields.offset(at)),
    };
    match offset {
        Some(offset) => h.part(b"offset").update(&offset.bytes().to_le_bytes()),
        None => h.part(b"uninhabited"),
    };
    Ok(())
}

/// The interface hashes of the items of `items` named `name`, sorted.
fn named<'f>(
    db: &Db,
    items: &[(Name, AssocItemId)],
    name: NameHash,
    frame: &'f Frame,
) -> Result<Vec<ContentId, &'f Frame>, Unhashable> {
    let names = name_hashes(items.iter().map(|(it, _)| it.as_str()));
    let mut found = Vec::new_in(frame);
    for (_, &(_, it)) in names.into_iter().zip(items).filter(|&(it, _)| it == name) {
        found.push(hash::interface_hash(db, ItemId::from(it))?.0);
    }
    found.sort_unstable();
    Ok(found)
}

/// The inherent impls for the type with an item of the name, and the traits with an item of
/// the name with whether the type implements them: every candidate method probing may find.
fn write_method(
    db: &Db,
    h: &mut Hasher,
    self_ty: SimplifiedType,
    name: NameHash,
) -> Result<(), Unhashable> {
    frame::task(Tag::Other, |frame| -> Result<(), Unhashable> {
        let mut inherent: Vec<_, &Frame> = Vec::new_in(frame);
        for (_, imp) in inherent_impls_of(db, self_ty, ()).iter() {
            let items = named(db, &imp.impl_items(db).items, name, frame)?;
            if !items.is_empty() {
                inherent.push((hash::interface_hash(db, ItemId::Impl(imp))?.0, items));
            }
        }
        inherent.sort_unstable();
        let mut traits: Vec<_, &Frame> = Vec::new_in(frame);
        for (_, trait_) in traits_with(db, name, ()).iter() {
            let items = named(db, &trait_.trait_items(db).items, name, frame)?;
            traits.push((items, implements_value(db, IMPLEMENTS, trait_, self_ty)?));
        }
        traits.sort_unstable();
        h.part(b"inherent").update(&(inherent.len() as u64).to_le_bytes());
        for (imp, items) in inherent.iter() {
            h.id(imp).update(&(items.len() as u64).to_le_bytes());
            for it in items.iter() {
                h.id(it);
            }
        }
        h.part(b"traits").update(&(traits.len() as u64).to_le_bytes());
        for (items, implements) in traits.iter() {
            h.update(&(items.len() as u64).to_le_bytes());
            for it in items.iter() {
                h.id(it);
            }
            h.id(implements);
        }
        Ok(())
    })
}

/// A const's value: evaluated for one without generics, else its code (each instance is
/// evaluated where it is used, from that code).
fn write_const_value(
    db: &Db,
    h: &mut Hasher,
    konst: ConstId,
) -> Result<(), Unhashable> {
    let owner = ItemId::Const(konst);
    if generics(db, konst.into()).iter(true).next().is_some() {
        h.part(b"generic");
        write_code(h, hash::code_hash(db, owner)?.map(|it| it.0));
        return Ok(());
    }
    let value = match db.const_eval(konst, GenericArgs::empty(), None) {
        Ok(value) => value,
        Err(err) => {
            scratch::with_string(|kind| {
                eval_error_kind(&err, kind);
                h.part(b"error").part(kind.as_bytes());
            });
            return Ok(());
        }
    };
    h.part(b"value").id(&ty_hash(db, owner, value.ty)?).part(&value.memory);
    match &value.memory_map {
        MemoryMap::Empty => {
            h.part(b"empty");
        }
        MemoryMap::Simple(bytes) => {
            h.part(b"simple").part(bytes);
        }
        MemoryMap::Complex(map) => frame::task(Tag::Other, |frame| -> Result<(), Unhashable> {
            h.part(b"complex");
            let mut blocks: Vec<(usize, &[u8]), &Frame> =
                Vec::with_capacity_in(map.memory.len(), frame);
            blocks.extend(map.memory.iter().map(|(&at, bytes)| (at, &bytes[..])));
            radix::sort_copy_by_key(&mut blocks, |&(at, _)| at);
            h.update(&(blocks.len() as u64).to_le_bytes());
            for &(at, bytes) in blocks.iter() {
                h.update(&(at as u64).to_le_bytes()).part(bytes);
                match value.memory_map.align_at(at) {
                    Some(align) => h.update(&[1]).update(&(align as u64).to_le_bytes()),
                    None => h.update(&[0]),
                };
            }
            h.update(&(map.statics.len() as u64).to_le_bytes());
            for it in &map.statics {
                h.update(&(it.start as u64).to_le_bytes())
                    .update(&(it.size as u64).to_le_bytes())
                    .id(&key::item_key(db, ItemId::Static(it.id)).0);
            }
            let mut vtables = Vec::with_capacity_in(map.vtable.tys().len(), frame);
            if !map.vtable.is_empty() {
                h.update(&(hir_ty::mir::VTableMap::FIRST_ID as u64).to_le_bytes());
                for ty in map.vtable.tys() {
                    vtables.push(ty_hash(db, owner, ty)?);
                }
            }
            h.update(&(vtables.len() as u64).to_le_bytes());
            for it in vtables.iter() {
                h.id(it);
            }
            h.update(&(map.type_ids.len() as u64).to_le_bytes());
            for it in &map.type_ids {
                match it {
                    Some(ty) => h.update(&[1]).id(&ty_hash(db, owner, *ty)?),
                    None => h.update(&[0]),
                };
            }
            Ok(())
        })?,
    }
    Ok(())
}

fn ty_hash<'db>(
    db: &'db Db,
    owner: ItemId,
    ty: Ty<'db>,
) -> Result<ContentId, Unhashable> {
    let sub = crate::form::Cx::new(db).ty(ty);
    hash::filled(db, CONST_TYPE, sub, owner)
}

/// Which error stopped an evaluation, written to `out`: its stage and kind, never its text,
/// which names spans.
fn eval_error_kind(err: &ConstEvalError<'_>, out: &mut String) {
    let stage = match err {
        ConstEvalError::MirLowerError(_) => "lower",
        ConstEvalError::MirEvalError(_) => "eval",
    };
    out.push_str(stage);
    out.push(' ');
    let start = out.len();
    let written = match err {
        ConstEvalError::MirLowerError(it) => write!(out, "{it:?}"),
        ConstEvalError::MirEvalError(it) => write!(out, "{it:?}"),
    };
    written.expect("writing to a string cannot fail");
    let kind = out[start..]
        .find(|c: char| !(c.is_alphanumeric() || c == '_'))
        .map_or(out.len(), |end| start + end);
    out.truncate(kind);
}

fn name_of(db: &Db, h: &mut Hasher, id: ItemId) {
    key::path_part(db, id, &mut |bytes| {
        h.update(bytes);
    });
    let owner: hir_def::AttrDefId = match id {
        ItemId::Function(it) => it.into(),
        ItemId::Adt(it) => it.into(),
        ItemId::Trait(it) => it.into(),
        ItemId::Impl(it) => it.into(),
        ItemId::Const(it) => it.into(),
        ItemId::Static(it) => it.into(),
        ItemId::Alias(it) => it.into(),
        ItemId::Macro(it) => it.into(),
    };
    h.update(&[AttrFlags::query(db, owner).contains(AttrFlags::NO_MANGLE) as u8]);
    write_symbol(h, AttrFlags::export_name(db, owner));
    write_symbol(h, AttrFlags::link_name(db, owner));
}

/// What inferred the closure at `expr` of `store`: the innermost anonymous const it is in, else
/// the body.
fn closure_owner<'db>(
    db: &'db Db,
    store: ExpressionStoreOwnerId,
    expr: ExprId,
) -> InferBodyId<'db> {
    let innermost = frame::task(Tag::Other, |frame| {
        let mut anon_consts: Vec<AnonConstId<'db>, &Frame> = Vec::new_in(frame);
        match store {
            ExpressionStoreOwnerId::Body(body) => {
                anon_consts.extend_from_slice(InferenceResult::of(db, body).defined_anon_consts())
            }
            ExpressionStoreOwnerId::Signature(def) => {
                anon_consts.extend(AnonConstId::all_from_signature(db, def))
            }
            ExpressionStoreOwnerId::VariantFields(variant) => anon_consts
                .extend_from_slice(db.field_types_with_diagnostics(variant).defined_anon_consts()),
        }
        let exprs = ExpressionStore::of(db, store);
        let mut stack = Vec::new_in(frame);
        anon_consts
            .iter()
            .filter(|anon| anon.loc(db).owner == store)
            .filter_map(|&anon| {
                Some((subtree_containing(exprs, anon.loc(db).expr, expr, &mut stack)?, anon))
            })
            .min_by_key(|&(size, _)| size)
    });
    match (innermost, store) {
        (Some((_, anon)), _) => InferBodyId::AnonConstId(anon),
        (None, ExpressionStoreOwnerId::Body(body)) => InferBodyId::DefWithBodyId(body),
        (None, _) => unreachable!("closure {expr:?} of {store:?} is in no anonymous const"),
    }
}

/// The number of expressions under `root` if `target` is one of them; `stack` is left empty.
fn subtree_containing<A: Allocator>(
    store: &ExpressionStore,
    root: ExprId,
    target: ExprId,
    stack: &mut Vec<ExprId, A>,
) -> Option<usize> {
    let (mut size, mut found) = (0, false);
    stack.push(root);
    while let Some(at) = stack.pop() {
        size += 1;
        found |= at == target;
        store.walk_child_exprs(at, |child| stack.push(child));
    }
    found.then_some(size)
}

fn infer_owner_item(db: &Db, owner: InferBodyId<'_>) -> ItemId {
    match owner {
        InferBodyId::DefWithBodyId(body) => ItemId::of_body(db, body),
        InferBodyId::AnonConstId(anon) => key::generic_def_item(anon.loc(db).owner.generic_def(db)),
    }
}

/// The orphan and coherence checks' results for `imp`, and every impl it could overlap: those
/// of its trait for its simplified self type and the blanket ones, all of the trait's for a
/// blanket impl, or the inherent impls for its self type.
fn write_coherence(db: &Db, h: &mut Hasher, imp: ImplId) -> Result<(), Unhashable> {
    let self_ty = impl_self_ty(db, imp);
    match db.impl_trait(imp) {
        Some(trait_ref) => {
            let trait_ = trait_ref.skip_binder().def_id.0;
            h.part(b"trait").update(&[check_orphan_rules(db, imp) as u8]);
            match self_ty {
                Some(self_ty) => h.id(&implements_value(db, IMPLEMENTS, trait_, self_ty)?),
                None => h.part(b"blanket").id(&all_impls_value(db, trait_)?),
            };
        }
        None => {
            let def_map = imp.lookup(db).container.def_map(db);
            h.part(b"inherent").update(&[is_inherent_impl_coherent(db, def_map, imp) as u8]);
            match self_ty {
                Some(self_ty) => {
                    let all = inherent_impls_of(db, self_ty, ())
                        .iter()
                        .map(|(key, it)| (key, AnyImpl::Impl(it)));
                    h.update(&[1]).id(&entry(db, all)?.value())
                }
                None => h.update(&[0]),
            };
        }
    }
    Ok(())
}

/// The type, its `Drop` impl if it has one, and that impl's `#[may_dangle]` parameters.
fn write_drop_check(db: &Db, h: &mut Hasher, adt: AdtId) -> Result<(), Unhashable> {
    h.id(&hash::interface_hash(db, ItemId::Adt(adt))?.0);
    let Some(imp) = hir_ty::drop::destructor(db, adt) else {
        h.part(b"no drop");
        return Ok(());
    };
    h.part(b"drop").id(&hash::interface_hash(db, ItemId::Impl(imp))?.0);
    let (lifetimes, types) = AttrFlags::query_generic_params(db, imp.into());
    for (param, flags) in lifetimes.iter() {
        if flags.contains(AttrFlags::MAY_DANGLE) {
            h.part(b"lifetime").update(&u32::from(param.into_raw()).to_le_bytes());
        }
    }
    for (param, flags) in types.iter() {
        if flags.contains(AttrFlags::MAY_DANGLE) {
            h.part(b"type").update(&u32::from(param.into_raw()).to_le_bytes());
        }
    }
    Ok(())
}

/// Every impl a derive on `adt` wrote, sorted by trait key: the built-in derives' and those in
/// the expansions of derive macros, at the top of the expansion or in the blocks of its
/// unnamed consts (`const _: () = { impl .. };`).
fn write_derive(db: &Db, h: &mut Hasher, adt: AdtId) -> Result<(), Unhashable> {
    let at: AstId<ast::Adt> = match adt {
        AdtId::StructId(it) => it.lookup(db).id.map(|it| it.upcast()),
        AdtId::EnumId(it) => it.lookup(db).id.map(|it| it.upcast()),
        AdtId::UnionId(it) => it.lookup(db).id.map(|it| it.upcast()),
    };
    let module = adt.module(db);
    let scope = module.def_map(db).module(db, module).scope(db);
    frame::task(Tag::Other, |frame| write_derived(db, h, scope, at, frame))
}

fn write_derived(
    db: &Db,
    h: &mut Hasher,
    scope: &ItemScope,
    at: AstId<ast::Adt>,
    frame: &Frame,
) -> Result<(), Unhashable> {
    let mut wrote: Vec<(Option<ItemKey>, InterfaceHash), &Frame> = Vec::new_in(frame);
    for (ast, invocations) in scope.derive_macro_invocs() {
        if ast != at {
            continue;
        }
        for (_, _, calls) in invocations {
            for call in calls.iter().flatten() {
                match *call {
                    Either::Left(call) => expansion_impls(db, scope, call, &mut wrote)?,
                    Either::Right(builtin) => {
                        let loc = builtin.loc(db);
                        let lang = lang_items(db, loc.adt.krate(db));
                        // hir-ty's `TraitImpls` has no impl for a derive whose trait is not a
                        // lang item of the crate.
                        let Some(trait_) = loc.trait_.get_id(lang) else { continue };
                        let trait_key = key::item_key(db, ItemId::Trait(trait_));
                        let interface = impl_interface(db, AnyImpl::BuiltinDerive(builtin))?;
                        wrote.push((Some(trait_key), interface));
                    }
                }
            }
        }
    }
    wrote.sort_unstable();
    h.update(&(wrote.len() as u64).to_le_bytes());
    for (trait_, interface) in wrote.iter() {
        match trait_ {
            Some(trait_key) => h.update(&[1]).id(&trait_key.0),
            None => h.update(&[0]),
        };
        h.id(&interface.0);
    }
    Ok(())
}

fn expansion_impls<A: Allocator>(
    db: &Db,
    scope: &ItemScope,
    call: MacroCallId,
    wrote: &mut Vec<(Option<ItemKey>, InterfaceHash), A>,
) -> Result<(), Unhashable> {
    let mut push = |imp: ImplId| -> Result<(), Unhashable> {
        let trait_ = db
            .impl_trait(imp)
            .map(|it| key::item_key(db, ItemId::Trait(it.skip_binder().def_id.0)));
        wrote.push((trait_, hash::interface_hash(db, ItemId::Impl(imp))?));
        Ok(())
    };
    for imp in scope.impls() {
        if written_by(db, imp.lookup(db).id.file_id, call) {
            push(imp)?;
        }
    }
    for konst in scope.unnamed_consts() {
        if !written_by(db, konst.lookup(db).id.file_id, call) {
            continue;
        }
        for (_, block) in Body::of(db, DefWithBodyId::from(konst)).blocks(db) {
            for (_, data) in block.modules(db) {
                for imp in data.scope(db).impls() {
                    push(imp)?;
                }
            }
        }
    }
    Ok(())
}

/// Whether `file` is the expansion of `call` or of a macro call inside it.
fn written_by(db: &Db, mut file: HirFileId, call: MacroCallId) -> bool {
    while let HirFileId::MacroFile(inner) = file {
        if inner == call {
            return true;
        }
        file = inner.loc(db).kind.file_id();
    }
    false
}
