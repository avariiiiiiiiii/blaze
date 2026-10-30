//! The full identity of a type, constant or argument list, spelled by content: what tells two
//! types apart where the program can (a `TypeId`, a `type_name`, a vtable, a `dyn` call, a
//! closure's body). It says at least what the code generator's own spelling of an instance's
//! arguments does (`hir-ty-codegen`'s `v2/hashes.rs`): an ADT by its key, its interface, its
//! layout and the `Drop` impls its drop glue reads; a coroutine by its code. So two places
//! with one identity are one thing to the code generator, in this revision and after an edit.

use base_db::Db;
use blaze_ids::{ContentId, Hasher};
use blaze_items::{
    ItemId, ItemKey, Unhashable, coroutine_code, interface_hash, item_key, key, layout_hash,
};
use either::Either;
use hir_def::{AdtId, CallableDefId, VariantId};
use hir_ty::{
    db::HirDatabase,
    next_solver::{Binder, Const, ConstKind, GenericArgs, Pattern, TermId, Ty, TyKind, ValTree},
};
use rustc_hash::FxHashSet;
use rustc_type_ir::{
    AliasTyKind, BoundTyKind, BoundVarIndexKind, ExistentialPredicate, GenericArgKind, InferConst,
    PatternKind, TermKind, ValTreeKind,
    inherent::{GenericArgs as _, IntoKind},
};
use smallvec::SmallVec;

use crate::{ADT_DOMAIN, IDENTITY_DOMAIN, Unshared};

pub(crate) fn ty<'db>(db: &'db Db, ty: Ty<'db>) -> Result<ContentId, Unshared> {
    let mut spell = Spell::new(db);
    spell.ty(ty)?;
    Ok(spell.out.finish())
}

pub(crate) fn args<'db>(
    db: &'db Db,
    args: GenericArgs<'db>,
) -> Result<ContentId, Unshared> {
    let mut spell = Spell::new(db);
    spell.args(args)?;
    Ok(spell.out.finish())
}

pub(crate) fn konst<'db>(
    db: &'db Db,
    konst: Const<'db>,
) -> Result<ContentId, Unshared> {
    let mut spell = Spell::new(db);
    spell.konst(konst)?;
    Ok(spell.out.finish())
}

pub(crate) fn unhashable(db: &Db) -> impl Fn(Unhashable) -> Unshared + '_ {
    move |it| Unshared::Unhashable(it.describe(db))
}

fn no_content(what: String) -> Unshared {
    Unshared::NoContent(what)
}

/// An ADT as its identity says it: its key, its interface (fields, variants, `repr`), its
/// layout, and the `Drop` impls of it and of every ADT its fields' types name, transitively.
#[salsa::tracked(returns(ref))]
pub(crate) fn adt(db: &Db, adt: AdtId) -> Result<ContentId, Unshared> {
    let item = ItemId::Adt(adt);
    let mut h = Hasher::new(ADT_DOMAIN);
    h.id(&item_key(db, item).0)
        .id(&interface_hash(db, item).map_err(unhashable(db))?.0)
        .id(&layout_hash(db, adt).map_err(unhashable(db))?.0);
    let mut seen: FxHashSet<AdtId> = FxHashSet::default();
    seen.insert(adt);
    // alloc: the ADTs the fields reach, once per ADT: this query's miss, memoized after
    let mut next = vec![adt];
    // alloc: as `next`; sorted, because the walk's order is not the ADT's
    let mut dropping: Vec<(ItemKey, ContentId)> = Vec::new();
    while let Some(at) = next.pop() {
        if let Some(imp) = hir_ty::drop::destructor(db, at) {
            let interface = interface_hash(db, ItemId::Impl(imp)).map_err(unhashable(db))?;
            dropping.push((item_key(db, ItemId::Adt(at)), interface.0));
        }
        for variant in variants(db, at) {
            for field in db.field_types(variant).values() {
                for arg in field.ty().skip_binder().walk() {
                    if let Some(ty) = arg.ty()
                        && let TyKind::Adt(def, _) = ty.kind()
                        && seen.insert(def.def_id())
                    {
                        next.push(def.def_id());
                    }
                }
            }
        }
    }
    dropping.sort_unstable();
    h.update(&(dropping.len() as u64).to_le_bytes());
    for (key, interface) in &dropping {
        h.id(&key.0).id(interface);
    }
    Ok(h.finish())
}

/// The variants of `adt` in declaration order: one for a struct or union.
pub(crate) fn variants<'db>(
    db: &'db Db,
    adt: AdtId,
) -> impl Iterator<Item = VariantId> + 'db {
    match adt {
        AdtId::StructId(it) => Either::Left(std::iter::once(it.into())),
        AdtId::UnionId(it) => Either::Left(std::iter::once(it.into())),
        AdtId::EnumId(it) => Either::Right(
            it.enum_variants(db).variants.iter().map(|(_, (variant, _))| VariantId::from(*variant)),
        ),
    }
}

/// A type or constant spelled by content straight into its hash: nothing is buffered.
pub(crate) struct Spell<'db> {
    db: &'db Db,
    pub(crate) out: Hasher,
}

/// `fmt::Write` into a spelling: a `Debug` name hashed without a `String`.
struct Text<'a>(&'a mut Hasher);

impl std::fmt::Write for Text<'_> {
    fn write_str(&mut self, text: &str) -> std::fmt::Result {
        self.0.update(text.as_bytes());
        Ok(())
    }
}

impl<'db> Spell<'db> {
    pub(crate) fn new(db: &'db Db) -> Spell<'db> {
        Spell { db, out: Hasher::new(IDENTITY_DOMAIN) }
    }

    fn u8(&mut self, value: u8) {
        self.out.update(&[value]);
    }

    fn u32(&mut self, value: u32) {
        self.out.update(&value.to_le_bytes());
    }

    fn len(&mut self, len: usize) {
        self.out.update(&(len as u64).to_le_bytes());
    }

    fn id(&mut self, id: &ContentId) {
        self.out.id(id);
    }

    fn str(&mut self, text: &str) {
        self.out.part(text.as_bytes());
    }

    /// `value`'s `Debug` text, ended by `0xff`, which no UTF-8 text holds.
    fn debug(&mut self, value: &impl std::fmt::Debug) {
        use std::fmt::Write as _;
        // `Text` never fails, so neither does the write.
        let _ = write!(Text(&mut self.out), "{value:?}");
        self.u8(0xff);
    }

    fn key(&mut self, key: ItemKey) {
        self.id(&key.0);
    }

    fn item(&mut self, item: ItemId) {
        self.key(item_key(self.db, item));
    }

    pub(crate) fn ty(&mut self, ty: Ty<'db>) -> Result<(), Unshared> {
        let db = self.db;
        match ty.kind() {
            TyKind::Bool => self.u8(0),
            TyKind::Char => self.u8(1),
            TyKind::Int(it) => {
                self.u8(2);
                self.str(it.name_str());
            }
            TyKind::Uint(it) => {
                self.u8(3);
                self.str(it.name_str());
            }
            TyKind::Float(it) => {
                self.u8(4);
                self.str(it.name_str());
            }
            TyKind::Adt(def, args) => {
                self.u8(5);
                let content = adt(db, def.def_id()).as_ref().map_err(Clone::clone)?;
                self.id(content);
                self.args(args)?;
            }
            TyKind::Foreign(it) => {
                self.u8(6);
                self.item(ItemId::Alias(it.0));
            }
            TyKind::Str => self.u8(7),
            TyKind::Array(element, len) => {
                self.u8(8);
                self.ty(element)?;
                self.konst(len)?;
            }
            TyKind::Pat(base, pattern) => {
                self.u8(9);
                self.ty(base)?;
                self.pattern(pattern)?;
            }
            TyKind::Slice(element) => {
                self.u8(10);
                self.ty(element)?;
            }
            TyKind::RawPtr(pointee, mutability) => {
                self.u8(11);
                self.u8(mutability.is_mut() as u8);
                self.ty(pointee)?;
            }
            TyKind::Ref(_, pointee, mutability) => {
                self.u8(12);
                self.u8(mutability.is_mut() as u8);
                self.ty(pointee)?;
            }
            TyKind::FnDef(def, args) => {
                let args = args.no_bound_vars().unwrap();
                self.u8(13);
                match def.0 {
                    CallableDefId::FunctionId(it) => {
                        self.u8(0);
                        self.item(ItemId::Function(it));
                    }
                    CallableDefId::StructId(it) => {
                        self.u8(1);
                        self.item(ItemId::Adt(it.into()));
                    }
                    CallableDefId::EnumVariantId(it) => {
                        self.u8(2);
                        self.key(key::variant_key(db, it));
                    }
                }
                self.args(args)?;
            }
            TyKind::FnPtr(tys, header) => {
                self.u8(14);
                self.len(tys.bound_vars().len());
                let tys = tys.skip_binder().inputs_and_output;
                self.len(tys.len());
                for ty in tys.iter() {
                    self.ty(ty)?;
                }
                self.debug(&header.abi());
                self.debug(&header.safety());
                self.u8(header.c_variadic() as u8);
            }
            TyKind::UnsafeBinder(inner) => {
                self.u8(15);
                let binder: Binder<'db, Ty<'db>> = inner.into();
                self.len(binder.bound_vars().len());
                self.ty(binder.skip_binder())?;
            }
            TyKind::Dynamic(predicates, _) => {
                self.u8(16);
                // The order the bounds are written in is not the type's.
                let mut each: SmallVec<[ContentId; 4]> = SmallVec::new();
                for predicate in predicates.iter() {
                    let mut inner = Spell::new(db);
                    inner.len(predicate.bound_vars().len());
                    match predicate.skip_binder() {
                        ExistentialPredicate::Trait(it) => {
                            inner.u8(0);
                            inner.item(ItemId::Trait(it.def_id.0));
                            inner.args(it.args)?;
                        }
                        ExistentialPredicate::Projection(it) => {
                            inner.u8(1);
                            match it.def_id.0 {
                                TermId::TypeAliasId(alias) => inner.item(ItemId::Alias(alias)),
                                TermId::ConstId(konst) => inner.item(ItemId::Const(konst)),
                            }
                            inner.args(it.args)?;
                            match it.term.kind() {
                                TermKind::Ty(ty) => {
                                    inner.u8(0);
                                    inner.ty(ty)?;
                                }
                                TermKind::Const(konst) => {
                                    inner.u8(1);
                                    inner.konst(konst)?;
                                }
                            }
                        }
                        ExistentialPredicate::AutoTrait(it) => {
                            inner.u8(2);
                            inner.item(ItemId::Trait(it.0));
                        }
                    }
                    each.push(inner.out.finish());
                }
                each.sort_unstable();
                self.len(each.len());
                for it in &each {
                    self.id(it);
                }
            }
            TyKind::Closure(id, args) => {
                self.u8(17);
                self.key(key::closure_key(db, id.0));
                self.args(args)?;
            }
            TyKind::CoroutineClosure(id, args) => {
                self.u8(18);
                self.key(key::coroutine_closure_key(db, id.0));
                self.args(args)?;
            }
            // A coroutine's layout is of the locals its body keeps across its yields, which
            // its arguments do not say: its code does.
            TyKind::Coroutine(id, args) => {
                self.u8(19);
                self.key(key::coroutine_key(db, id.0));
                self.id(&coroutine_code(db, id.0).map_err(unhashable(db))?.0);
                self.args(args)?;
            }
            TyKind::CoroutineWitness(id, args) => {
                self.u8(20);
                self.key(key::coroutine_key(db, id.0));
                self.id(&coroutine_code(db, id.0).map_err(unhashable(db))?.0);
                self.args(args)?;
            }
            TyKind::Never => self.u8(21),
            TyKind::Tuple(tys) => {
                self.u8(22);
                self.len(tys.len());
                for ty in tys.iter() {
                    self.ty(ty)?;
                }
            }
            TyKind::Param(param) => {
                self.u8(23);
                self.u32(param.index);
            }
            TyKind::Bound(index, bound) => {
                self.u8(24);
                self.bound_index(index);
                self.u32(bound.var.as_u32());
                self.u8(match bound.kind {
                    BoundTyKind::Anon => 0,
                    BoundTyKind::Param(_) => 1,
                });
            }
            TyKind::Alias(_, alias) => {
                let what = match alias.kind {
                    AliasTyKind::Projection { .. } => "a projection",
                    AliasTyKind::Inherent { .. } => "an inherent associated type",
                    AliasTyKind::Opaque { .. } => "an opaque type",
                    AliasTyKind::Free { .. } => "a type alias",
                };
                return Err(no_content(format!("{what} left unnormalized in `{ty:?}`"))); // alloc: error path
            }
            TyKind::Placeholder(_) => {
                return Err(no_content(format!("a placeholder type `{ty:?}`"))); // alloc: error path
            }
            TyKind::Infer(_) => {
                return Err(no_content(format!("an inference variable `{ty:?}`"))); // alloc: error path
            }
            TyKind::Error(_) => return Err(no_content("a type with an error".to_owned())), // alloc: error path
        }
        Ok(())
    }

    fn bound_index(&mut self, index: BoundVarIndexKind) {
        match index {
            BoundVarIndexKind::Bound(depth) => {
                self.u8(0);
                self.u32(depth.as_u32());
            }
            BoundVarIndexKind::Canonical => self.u8(1),
        }
    }

    pub(crate) fn args(&mut self, args: GenericArgs<'db>) -> Result<(), Unshared> {
        self.len(args.len());
        for arg in args.iter() {
            match arg.kind() {
                GenericArgKind::Lifetime(_) => self.u8(0),
                GenericArgKind::Type(ty) => {
                    self.u8(1);
                    self.ty(ty)?;
                }
                GenericArgKind::Const(konst) => {
                    self.u8(2);
                    self.konst(konst)?;
                }
            }
        }
        Ok(())
    }

    pub(crate) fn pattern(&mut self, pattern: Pattern<'db>) -> Result<(), Unshared> {
        match pattern.kind() {
            PatternKind::Range { start, end } => {
                self.u8(0);
                self.konst(start)?;
                self.konst(end)?;
            }
            PatternKind::Or(patterns) => {
                self.u8(1);
                self.len(patterns.len());
                for it in patterns.iter() {
                    self.pattern(it)?;
                }
            }
            PatternKind::NotNull => self.u8(2),
        }
        Ok(())
    }

    /// A constant by its value and its type's identity.
    pub(crate) fn konst(&mut self, konst: Const<'db>) -> Result<(), Unshared> {
        match konst.kind() {
            ConstKind::Value(value) => {
                self.u8(0);
                self.ty(value.ty)?;
                self.valtree(value.value)?;
            }
            ConstKind::Param(param) => {
                self.u8(1);
                self.u32(param.index);
            }
            ConstKind::Bound(index, bound) => {
                self.u8(2);
                self.bound_index(index);
                self.u32(bound.var.as_u32());
            }
            ConstKind::Alias(..) => {
                return Err(no_content(format!("the constant `{konst:?}` is not evaluated"))); // alloc: error path
            }
            ConstKind::Expr(_) => {
                return Err(no_content(format!("the constant expression `{konst:?}`"))); // alloc: error path
            }
            ConstKind::Infer(InferConst::Var(_) | InferConst::Fresh(_)) => {
                return Err(no_content(format!("an inference variable `{konst:?}`"))); // alloc: error path
            }
            ConstKind::Placeholder(_) => {
                return Err(no_content(format!("a placeholder constant `{konst:?}`"))); // alloc: error path
            }
            ConstKind::Error(_) => {
                return Err(no_content("a constant with an error".to_owned())); // alloc: error path
            }
        }
        Ok(())
    }

    fn valtree(&mut self, value: ValTree<'db>) -> Result<(), Unshared> {
        match value.kind() {
            ValTreeKind::Leaf(scalar) => {
                self.u8(0);
                self.out.update(&scalar.size().bytes().to_le_bytes());
                self.out.update(&scalar.to_bits_unchecked().to_le_bytes());
            }
            ValTreeKind::Branch(consts) => {
                self.u8(1);
                self.len(consts.len());
                for it in consts.iter() {
                    self.konst(it)?;
                }
            }
        }
        Ok(())
    }
}
