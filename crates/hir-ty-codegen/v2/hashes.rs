//! [`super::Hashes`] over the items lane's hashes ([`blaze_items`]): what the code generator
//! puts in place of the database's dense ids.
//!
//! - A body's code is its item's code hash, which takes each callee by its interface hash, with
//!   the ids of the positions its MIR names (the spans of [`ItemHashes::span`]) beside it: a
//!   template that names an expression by its id is never reused once that id is another
//!   expression's.
//! - A type is spelled by content into one buffer and hashed once: an ADT by its key, its
//!   interface, its layout and what drop glue reads of it ([`adt_content`], a tracked query),
//!   and its arguments the same way. Regions are left out.
//! - A position is a [`SpanRef`] into one of the tables of the body's owner, one per kind of
//!   span, indexed by the span's id in the owner's store: an edit elsewhere in the file moves no
//!   index, and a formatting edit changes the table only ([`ItemHashes::span_tables`]).
//!
//! Nothing here is shared between threads but salsa's memos: no lock on any path.

#[cfg(test)]
mod tests;

use base_db::Db;
use blaze_format::SpanRef;
use blaze_ids::{ContentId, Hasher};
use blaze_items::{
    CodeHash, ItemId, ItemKey, Unhashable, closure_code, code_hash, coroutine_code,
    interface_hash, item_key, key, layout_hash,
};
use hir_def::{
    AdtId, CallableDefId, DefWithBodyId, ExpressionStoreOwnerId, FunctionId, VariantId,
    expr_store::{Body as BodyStore, ExpressionStore},
};
use hir_ty::{
    InferBodyId, InferenceResult,
    db::{HirDatabase, InternedClosureId, InternedCoroutineId},
    mir::{MirBody, is_by_move_coroutine},
    next_solver::{Binder, Const, ConstKind, GenericArgs, Pattern, TermId, Ty, TyKind, ValTree},
};
use hir_ty_mir::mir::MirSpan;
use rustc_hash::FxHashSet;
use rustc_type_ir::{
    AliasTyKind, BoundTyKind, BoundVarIndexKind, ExistentialPredicate, GenericArgKind, InferConst,
    PatternKind, TermKind, ValTreeKind,
    inherent::{GenericArgs as _, IntoKind},
};
use salsa::plumbing::FromId;

use super::{Body, Hashes};

pub const TY_DOMAIN: &str = "blaze v2 codegen type 1";
pub const CONST_DOMAIN: &str = "blaze v2 codegen const 1";
pub const ADT_DOMAIN: &str = "blaze v2 codegen adt 1";
pub const CODE_DOMAIN: &str = "blaze v2 codegen code 1";
pub const DISCRIMINANT_DOMAIN: &str = "blaze v2 codegen discriminant 1";
pub const SPAN_DOMAIN: &str = "blaze v2 codegen span table 1";
pub const NAMES_DOMAIN: &str = "blaze v2 codegen binding names 1";

/// The tables of a body's spans, by the kind of span: [`MirSpan`]'s variants.
const EXPRS: usize = 0;
const EXPR_ENDS: usize = 1;
const PATS: usize = 2;
const BINDINGS: usize = 3;
const SELF_PARAM: usize = 4;
const UNKNOWN: usize = 5;
const TABLES: usize = 6;
/// The table of a function's declaration, apart from its body's.
const DECLARATION: u8 = TABLES as u8;

/// What a body's code is beside its code hash, in the preimage of [`CODE_DOMAIN`].
const ITEM: u8 = 0;
const CLOSURE: u8 = 1;
const COROUTINE: u8 = 2;
const BY_MOVE: u8 = 3;

/// The items lane's hashes as the code generator asks for them, and what the program calls
/// each file.
pub struct ItemHashes {
    names: Box<dyn Fn(base_db::FileId) -> Option<String> + Send + Sync>,
}

impl ItemHashes {
    /// `names` says what the program calls a file (`file!()`): its path relative to its
    /// package's root, `None` for a file the build did not name.
    pub fn new(names: Box<dyn Fn(base_db::FileId) -> Option<String> + Send + Sync>) -> ItemHashes {
        ItemHashes { names }
    }

    /// Every span table of `owner`'s body that [`Hashes::span`] names, by its anchor: the span
    /// at each index, for the linker's table to give the position of
    /// (`hir_ty_mir::mir::location::source_location_in_file`). A span that function finds no syntax for
    /// (a binding, the self parameter, [`MirSpan::Unknown`]) is where its owner is.
    pub fn span_tables(db: &Db, owner: DefWithBodyId) -> Vec<(ContentId, Vec<MirSpan>)> {
        let anchors = anchors(db, owner).0;
        let store = BodyStore::of(db, owner);
        let spans: [Vec<MirSpan>; TABLES] = [
            store.exprs().map(|(it, _)| MirSpan::ExprId(it)).collect(),
            store.exprs().map(|(it, _)| MirSpan::ExprEnd(it)).collect(),
            store.pats().map(|(it, _)| MirSpan::PatId(it)).collect(),
            store.bindings().map(|(it, _)| MirSpan::BindingId(it)).collect(),
            vec![MirSpan::SelfParam],
            vec![MirSpan::Unknown],
        ];
        anchors.into_iter().zip(spans).collect()
    }

    /// The anchor of the table [`Hashes::declaration`] names: one span, where `func` is
    /// declared.
    pub fn declaration_table(db: &Db, func: FunctionId) -> ContentId {
        let mut h = Hasher::new(SPAN_DOMAIN);
        h.id(&item_key(db, ItemId::Function(func)).0).update(&[DECLARATION]);
        h.finish()
    }
}

impl Hashes for ItemHashes {
    fn item(&self, db: &Db, item: ItemId) -> Result<ItemKey, String> {
        Ok(item_key(db, item))
    }

    fn code(&self, db: &Db, body: Body) -> Result<CodeHash, String> {
        hir_ty::attach_db(db, || {
            let code = match body {
                Body::Item(def) => item_code(db, def),
                Body::Closure(id) => closure_code_in(db, InternedClosureId::from_id(id)),
                Body::Coroutine(id) => coroutine_code_in(db, InternedCoroutineId::from_id(id)),
                Body::CoroutineByMove(id) => by_move_code_in(db, InternedCoroutineId::from_id(id)),
            };
            code.clone().map(CodeHash)
        })
    }

    fn ty<'db>(&self, db: &'db Db, ty: Ty<'db>) -> Result<ContentId, String> {
        hir_ty::attach_db(db, || {
            let mut spell = Spell::new(db);
            spell.ty(ty)?;
            Ok(ContentId::of(TY_DOMAIN, &spell.out))
        })
    }

    fn konst<'db>(&self, db: &'db Db, konst: Const<'db>) -> Result<ContentId, String> {
        hir_ty::attach_db(db, || {
            let mut spell = Spell::new(db);
            spell.konst(konst)?;
            Ok(ContentId::of(CONST_DOMAIN, &spell.out))
        })
    }

    fn span(
        &self,
        db: &Db,
        owner: DefWithBodyId,
        span: MirSpan,
    ) -> Result<SpanRef, String> {
        let Ok((table, index)) = table_of(span) else {
            return Err(format!(
                "{span:?} of the body of {owner:?}, a span of code inlined into it, which is of \
                 the body it was inlined from: its place is the inlined span's"
            ));
        };
        Ok(SpanRef { anchor: anchors(db, owner).0[table], index })
    }

    fn declaration(&self, db: &Db, func: FunctionId) -> Result<SpanRef, String> {
        Ok(SpanRef { anchor: ItemHashes::declaration_table(db, func), index: 0 })
    }

    fn file_name(&self, _: &Db, file: base_db::FileId) -> Result<String, String> {
        (self.names)(file).ok_or_else(|| {
            format!(
                "the file {file:?} has no name in the program: the build did not name it, so \
                 `file!()` and the positions of its code have none"
            )
        })
    }
}

/// Which table of its owner's a span is in, and its index there; for a span of code inlined
/// into the owner's body, which is of another's, its index in `Inlined::spans`.
fn table_of(span: MirSpan) -> Result<(usize, u32), u32> {
    Ok(match span {
        MirSpan::ExprId(it) => (EXPRS, it.into_raw().into_u32()),
        MirSpan::ExprEnd(it) => (EXPR_ENDS, it.into_raw().into_u32()),
        MirSpan::PatId(it) => (PATS, it.into_raw().into_u32()),
        MirSpan::BindingId(it) => (BINDINGS, it.into_raw().into_u32()),
        MirSpan::SelfParam => (SELF_PARAM, 0),
        MirSpan::Unknown => (UNKNOWN, 0),
        MirSpan::Inlined(at) => return Err(at),
    })
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Anchors([ContentId; TABLES]);

/// The anchors of a body's span tables: of the key of its store, which an edit of what the
/// body is keeps.
#[salsa::tracked(returns(copy))]
fn anchors(db: &Db, owner: DefWithBodyId) -> Anchors {
    let store = key::store_key(db, ExpressionStoreOwnerId::Body(owner));
    Anchors(std::array::from_fn(|table| {
        let mut h = Hasher::new(SPAN_DOMAIN);
        h.id(&store.0).update(&[table as u8]);
        h.finish()
    }))
}

fn unhashable(db: &Db) -> impl Fn(Unhashable) -> String + '_ {
    move |it| it.describe(db)
}

/// The code hash beside what a template built from `body` holds of it that the code hash
/// leaves out: the names of its bindings, which its debug info tells, and the positions it
/// names, by table and index, in the order of its blocks.
fn with_spans(
    db: &Db,
    kind: u8,
    parts: &[ContentId],
    body: &MirBody<'_>,
) -> ContentId {
    let mut out = Vec::with_capacity(1 + 32 * (parts.len() + 1) + 5 * 8 * body.basic_blocks.len());
    out.push(kind);
    for part in parts {
        out.extend_from_slice(&part.0);
    }
    out.extend_from_slice(&names(db, body).0);
    let push = |out: &mut Vec<u8>, span: Option<MirSpan>| match span.map(table_of) {
        Some(Ok((table, index))) => {
            out.push(table as u8);
            out.extend_from_slice(&index.to_le_bytes());
        }
        Some(Err(at)) => {
            out.push(0xfe);
            out.extend_from_slice(&at.to_le_bytes());
        }
        None => out.push(0xff),
    };
    for (_, block) in body.basic_blocks.iter() {
        for statement in &block.statements {
            push(&mut out, Some(statement.span));
            statement.kind.for_each_operand(&mut |it| push(&mut out, it.span));
        }
        if let Some(terminator) = &block.terminator {
            push(&mut out, Some(terminator.span));
            terminator.kind.for_each_operand(&mut |it| push(&mut out, it.span));
        }
    }
    // The spans of inlined code, each of the body of its scope's callee, and where each
    // scope was inlined at.
    let inlined = &body.inlined;
    out.extend_from_slice(&(inlined.spans.len() as u32).to_le_bytes());
    for span in &inlined.spans {
        out.extend_from_slice(&span.scope.to_le_bytes());
        push(&mut out, Some(span.span));
    }
    out.extend_from_slice(&(inlined.scopes.len() as u32).to_le_bytes());
    for scope in &inlined.scopes {
        out.extend_from_slice(&scope.callee.to_le_bytes());
        push(&mut out, Some(scope.call_site));
    }
    ContentId::of(CODE_DOMAIN, &out)
}

/// The name of each of `body`'s bindings, in the order of their locals, each framed by its
/// local and its length.
fn names(db: &Db, body: &MirBody<'_>) -> ContentId {
    let store = ExpressionStore::of(db, body.owner.expression_store_owner(db));
    let mut out = Vec::new();
    for (local, binding) in body.local_to_binding_map().iter() {
        let name = store[*binding].name.as_str();
        out.extend_from_slice(&local.into_raw().into_u32().to_le_bytes());
        out.extend_from_slice(&(name.len() as u32).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
    }
    ContentId::of(NAMES_DOMAIN, &out)
}

fn no_mir(what: impl std::fmt::Debug, error: impl std::fmt::Debug) -> String {
    format!("the MIR of {what:?} failed to lower: {error:?}")
}

/// The code of a function, constant or static: its item's code hash. An enum variant's
/// discriminant is a body of its enum's interface.
#[salsa::tracked(returns(ref))]
fn item_code(db: &Db, def: DefWithBodyId) -> Result<ContentId, String> {
    let item = ItemId::of_body(db, def);
    let code = match def {
        DefWithBodyId::VariantId(variant) => {
            let mut h = Hasher::new(DISCRIMINANT_DOMAIN);
            h.id(&key::variant_key(db, variant).0)
                .id(&interface_hash(db, item).map_err(unhashable(db))?.0);
            h.finish()
        }
        DefWithBodyId::FunctionId(_) | DefWithBodyId::ConstId(_) | DefWithBodyId::StaticId(_) => {
            match code_hash(db, item).map_err(unhashable(db))? {
                Some(code) => code.0,
                None => {
                    return Err(format!(
                        "blaze-items: {:?} `{}` has no body to compile",
                        item.kind(),
                        key::path(db, item)
                    ));
                }
            }
        }
    };
    let body = db.mir_body(InferBodyId::DefWithBodyId(def)).map_err(|it| no_mir(def, it))?;
    Ok(with_spans(db, ITEM, &[code], body))
}

#[salsa::tracked(returns(ref))]
fn closure_code_in<'db>(
    db: &'db Db,
    closure: InternedClosureId<'db>,
) -> Result<ContentId, String> {
    let code = closure_code(db, closure).map_err(unhashable(db))?;
    let body = db.mir_body_for_closure(closure).map_err(|it| no_mir(closure, it))?;
    Ok(with_spans(db, CLOSURE, &[code.0], body))
}

#[salsa::tracked(returns(ref))]
fn coroutine_code_in<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
) -> Result<ContentId, String> {
    let code = coroutine_code(db, coroutine).map_err(unhashable(db))?;
    let body = db.mir_body_for_coroutine(coroutine).map_err(|it| no_mir(coroutine, it))?;
    Ok(with_spans(db, COROUTINE, &[code.0], body))
}

/// The future an `async` closure gives when called by value: the coroutine's body lowered
/// against the closure's captures, so its code is the coroutine's with what the closure
/// captures and how.
#[salsa::tracked(returns(ref))]
fn by_move_code_in<'db>(
    db: &'db Db,
    coroutine: InternedCoroutineId<'db>,
) -> Result<ContentId, String> {
    let code = coroutine_code(db, coroutine).map_err(unhashable(db))?;
    let loc = coroutine.loc(db);
    let closure = ExpressionStore::closure_for_coroutine(loc.expr);
    let closure = match InferenceResult::of(db, loc.owner).expr_ty(closure).kind() {
        TyKind::CoroutineClosure(id, _) => id.0.as_closure(db),
        other => {
            return Err(format!(
                "the coroutine {coroutine:?}, run by move, is of no `async` closure but of {other:?}"
            ));
        }
    };
    let captures =
        blaze_items::value(db, &blaze_items::captures(db, closure)).map_err(unhashable(db))?;
    let body =
        db.mir_body_for_coroutine_by_move(coroutine).map_err(|it| no_mir(coroutine, it))?;
    Ok(with_spans(db, BY_MOVE, &[code.0, captures], body))
}

/// An ADT as a type's content says it: its key, its interface (fields, variants, `repr`), its
/// layout, and the `Drop` impls of it and of every ADT its fields' types name, transitively,
/// which drop glue reads.
#[salsa::tracked(returns(ref))]
fn adt_content(db: &Db, adt: AdtId) -> Result<ContentId, String> {
    let item = ItemId::Adt(adt);
    let mut h = Hasher::new(ADT_DOMAIN);
    h.id(&item_key(db, item).0)
        .id(&interface_hash(db, item).map_err(unhashable(db))?.0)
        .id(&layout_hash(db, adt).map_err(unhashable(db))?.0);
    let mut seen: FxHashSet<AdtId> = FxHashSet::default();
    seen.insert(adt);
    let mut next = vec![adt];
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

fn variants(db: &Db, adt: AdtId) -> Vec<VariantId> {
    match adt {
        AdtId::StructId(it) => vec![it.into()],
        AdtId::UnionId(it) => vec![it.into()],
        AdtId::EnumId(it) => it
            .enum_variants(db)
            .variants
            .iter()
            .map(|(_, (variant, _))| VariantId::from(*variant))
            .collect(),
    }
}

/// A type or constant spelled by content into one buffer, hashed once by the caller.
struct Spell<'db> {
    db: &'db Db,
    out: Vec<u8>,
}

impl<'db> Spell<'db> {
    fn new(db: &'db Db) -> Spell<'db> {
        Spell { db, out: Vec::with_capacity(256) }
    }

    fn u8(&mut self, value: u8) {
        self.out.push(value);
    }

    fn u32(&mut self, value: u32) {
        self.out.extend_from_slice(&value.to_le_bytes());
    }

    fn len(&mut self, len: usize) {
        self.out.extend_from_slice(&(len as u64).to_le_bytes());
    }

    fn id(&mut self, id: &ContentId) {
        self.out.extend_from_slice(&id.0);
    }

    fn str(&mut self, text: &str) {
        self.len(text.len());
        self.out.extend_from_slice(text.as_bytes());
    }

    fn key(&mut self, key: ItemKey) {
        self.id(&key.0);
    }

    fn item(&mut self, item: ItemId) {
        self.key(item_key(self.db, item));
    }

    fn ty(&mut self, ty: Ty<'db>) -> Result<(), String> {
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
                let content = adt_content(db, def.def_id()).as_ref().map_err(Clone::clone)?;
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
                self.str(&format!("{:?} {:?}", header.abi(), header.safety()));
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
                let mut each = Vec::new();
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
                    each.push(ContentId::of(TY_DOMAIN, &inner.out));
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
                let code = match is_by_move_coroutine(db, id.0, args.as_coroutine()) {
                    true => by_move_code_in(db, id.0),
                    false => coroutine_code_in(db, id.0),
                };
                self.id(code.as_ref().map_err(Clone::clone)?);
                self.args(args)?;
            }
            TyKind::CoroutineWitness(id, args) => {
                self.u8(20);
                self.key(key::coroutine_key(db, id.0));
                self.id(coroutine_code_in(db, id.0).as_ref().map_err(Clone::clone)?);
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
            TyKind::Alias(alias) => {
                let what = match alias.kind {
                    AliasTyKind::Projection { .. } => "a projection",
                    AliasTyKind::Inherent { .. } => "an inherent associated type",
                    AliasTyKind::Opaque { .. } => "an opaque type",
                    AliasTyKind::Free { .. } => "a type alias",
                };
                return Err(format!("{what} left unnormalized in `{ty:?}`, which has no content"));
            }
            TyKind::Placeholder(_) => {
                return Err(format!("a placeholder type `{ty:?}`, which has no content"));
            }
            TyKind::Infer(_) => {
                return Err(format!("an inference variable `{ty:?}`, which has no content"));
            }
            TyKind::Error(_) => return Err("a type with an error, which has no content".to_owned()),
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

    fn args(&mut self, args: GenericArgs<'db>) -> Result<(), String> {
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

    fn pattern(&mut self, pattern: Pattern<'db>) -> Result<(), String> {
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

    /// A constant by its value and its type's content.
    fn konst(&mut self, konst: Const<'db>) -> Result<(), String> {
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
            ConstKind::Unevaluated(_) => {
                return Err(format!("the constant `{konst:?}` is not evaluated: it has no value"));
            }
            ConstKind::Expr(_) => {
                return Err(format!("the constant expression `{konst:?}` has no value"));
            }
            ConstKind::Infer(InferConst::Var(_) | InferConst::Fresh(_)) => {
                return Err(format!("an inference variable `{konst:?}`, which has no value"));
            }
            ConstKind::Placeholder(_) => {
                return Err(format!("a placeholder constant `{konst:?}`, which has no value"));
            }
            ConstKind::Error(_) => {
                return Err("a constant with an error, which has no value".to_owned());
            }
        }
        Ok(())
    }

    fn valtree(&mut self, value: ValTree<'db>) -> Result<(), String> {
        match value.kind() {
            ValTreeKind::Leaf(scalar) => {
                self.u8(0);
                self.out.extend_from_slice(&scalar.size().bytes().to_le_bytes());
                self.out.extend_from_slice(&scalar.to_bits_unchecked().to_le_bytes());
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
