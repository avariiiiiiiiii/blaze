//! An item's bound hash (`v2/requests/fastcompile.md`, fastcompile-ident-select 6): its code
//! hash and what its code binds to by identity, for test selection.
//!
//! A code hash fills each ref by its interface hash, so a call of `a` and a call of `b` with
//! one signature hash alike, as do two bodies swapped between them. The bound hash adds:
//!
//! - the key of each ref of the body, in slot order, and the deep hash of each constant it
//!   names, for its value;
//! - the key of what each trait method it names resolves to where the item is written
//!   (`lookup_impl_method`), sorted: a method left generic resolves to itself, and the
//!   instance's type arguments say the rest;
//! - for each trait object an unsize coercion makes, the key of the trait and of each of its
//!   supertraits with the value of every impl of it (`all_impls_value`), sorted: the methods
//!   the vtable holds are the impls' items of the trait's names, by key. Which impl the source
//!   type picks is left to that set, so it changes with any impl of the trait.
//!
//! It is no part of [`crate::CodeHash`]: codegen's memo is keyed as before.

use blaze_ids::{ContentId, Hasher};
use either::Either;
use hir_def::{CallableDefId, ItemContainerId, TraitId};
use hir_ty::{
    InferBodyId, PointerCast, all_super_traits,
    db::{HirDatabase, InternedClosureId, InternedCoroutineId},
    mir::{AggregateKind, CastKind, MirBody, Operand, OperandKind, Rvalue, StatementKind},
    next_solver::{Ty, TyKind},
    traits::ParamEnvAndCrate,
};
use rustc_hash::FxHashSet;
use rustc_type_ir::inherent::{BoundExistentialPredicates as _, IntoKind as _};

use crate::{ItemId, ItemKey, Unhashable, hash, index, key};

pub const BOUND_DOMAIN: &str = "blaze v2 bound hash 1";
const DERIVE_METHOD_DOMAIN: &str = "blaze v2 bound derive method 1";

/// The bound hash of an item with a body, `None` for one without.
pub fn bound_hash(db: &dyn HirDatabase, id: ItemId) -> Result<Option<ContentId>, Unhashable> {
    bound_of(db, id)
}

#[salsa::tracked(returns(copy))]
fn bound_of(db: &dyn HirDatabase, id: ItemId) -> Result<Option<ContentId>, Unhashable> {
    let Some(code) = hash::code_hash(db, id)? else { return Ok(None) };
    let (Some(body), Some(def)) = (id.body(), id.generic_def()) else {
        unreachable!("{id:?} has a code hash and no body")
    };
    let form = hash::body_form(db, id).as_ref().expect("an item with a code hash has a body form");
    let mut h = Hasher::new(BOUND_DOMAIN);
    h.id(&code.0).update(&(form.refs.len() as u64).to_le_bytes());
    for &to in form.refs.iter() {
        h.id(&key::item_key(db, to).0);
        match to {
            ItemId::Const(_) => h.update(&[1]).id(&hash::deep_hash(db, to)?.0),
            _ => h.update(&[0]),
        };
    }
    let env = ParamEnvAndCrate { param_env: db.trait_environment(def), krate: id.krate(db) };
    let reached = reached(db, id, InferBodyId::DefWithBodyId(body), env)?;
    h.update(&(reached.methods.len() as u64).to_le_bytes());
    for method in &reached.methods {
        h.id(&method.0);
    }
    h.update(&(reached.traits.len() as u64).to_le_bytes());
    for &(key, trait_) in &reached.traits {
        h.id(&key.0).id(&index::all_impls_value(db, trait_)?);
    }
    Ok(Some(h.finish()))
}

/// What the MIR of a body and of every closure and coroutine in it binds to beyond its refs.
struct Reached {
    /// What each trait method named resolves to, sorted, distinct.
    methods: Vec<ItemKey>,
    /// Each trait an unsize coercion makes an object of, and its supertraits, sorted by key,
    /// distinct.
    traits: Vec<(ItemKey, TraitId)>,
}

fn reached<'db>(
    db: &'db dyn HirDatabase,
    id: ItemId,
    body: InferBodyId<'db>,
    env: ParamEnvAndCrate<'db>,
) -> Result<Reached, Unhashable> {
    let no_mir = |_| Unhashable { item: id, what: "its MIR failed to lower" };
    let mut seen: FxHashSet<Either<InternedClosureId<'db>, InternedCoroutineId<'db>>> =
        FxHashSet::default();
    let mut work: Vec<&'db MirBody<'db>> = vec![db.mir_body(body).map_err(no_mir)?];
    let mut methods = Vec::new();
    let mut objects: Vec<TraitId> = Vec::new();
    while let Some(body) = work.pop() {
        let mut nested: Vec<Either<InternedClosureId<'db>, InternedCoroutineId<'db>>> =
            body.closures.iter().map(|&it| Either::Left(it)).collect();
        for (_, block) in body.basic_blocks.iter() {
            for statement in &block.statements {
                if let StatementKind::Assign(_, rvalue) = &statement.kind {
                    match rvalue {
                        Rvalue::Cast(CastKind::PointerCoercion(PointerCast::Unsize), _, ty) => {
                            objects_in(ty.as_ref(), &mut objects)
                        }
                        Rvalue::Aggregate(AggregateKind::Closure(ty), _) => {
                            match ty.as_ref().kind() {
                                TyKind::Closure(it, _) => nested.push(Either::Left(it.0)),
                                TyKind::CoroutineClosure(it, _) => {
                                    nested.push(Either::Left(it.0.as_closure(db)))
                                }
                                TyKind::Coroutine(it, _) => nested.push(Either::Right(it.0)),
                                _ => {}
                            }
                        }
                        _ => {}
                    }
                }
                statement.kind.for_each_operand(&mut |it| method(db, env, it, &mut methods));
            }
            if let Some(terminator) = &block.terminator {
                terminator.kind.for_each_operand(&mut |it| method(db, env, it, &mut methods));
            }
        }
        for it in nested {
            if seen.insert(it) {
                work.push(match it {
                    Either::Left(closure) => db.mir_body_for_closure(closure),
                    Either::Right(coroutine) => db.mir_body_for_coroutine(coroutine),
                }
                .map_err(no_mir)?);
            }
        }
    }
    methods.sort_unstable();
    methods.dedup();
    let mut traits: Vec<(ItemKey, TraitId)> = objects
        .into_iter()
        .flat_map(|it| all_super_traits(db, it).iter().copied())
        .map(|it| (key::item_key(db, ItemId::Trait(it)), it))
        .collect();
    traits.sort_unstable_by_key(|&(key, _)| key);
    traits.dedup_by_key(|&mut (key, _)| key);
    Ok(Reached { methods, traits })
}

/// The principal trait of every trait object in `ty`.
fn objects_in(ty: Ty<'_>, out: &mut Vec<TraitId>) {
    for arg in ty.walk() {
        if let Some(ty) = arg.ty()
            && let TyKind::Dynamic(predicates, _) = ty.kind()
            && let Some(principal) = predicates.principal_def_id()
        {
            out.push(principal.0);
        }
    }
}

/// What a trait method `operand` names resolves to: a function by its key, or a method of a
/// built-in derive by its impl's key and the method.
fn method<'db>(
    db: &'db dyn HirDatabase,
    env: ParamEnvAndCrate<'db>,
    operand: &Operand,
    out: &mut Vec<ItemKey>,
) {
    let ty = match &operand.kind {
        OperandKind::Constant { ty, .. } => ty.as_ref(),
        OperandKind::Allocation { allocation } => allocation.as_ref().ty,
        OperandKind::Copy(_) | OperandKind::Move(_) | OperandKind::Static(_) => return,
    };
    let TyKind::FnDef(def, args) = ty.kind() else { return };
    let CallableDefId::FunctionId(func) = def.0 else { return };
    if !matches!(func.loc(db).container, ItemContainerId::TraitId(_)) {
        return;
    }
    out.push(match db.lookup_impl_method(env, func, args).0 {
        Either::Left(it) => key::item_key(db, ItemId::Function(it)),
        Either::Right((imp, method)) => ItemKey(
            Hasher::new(DERIVE_METHOD_DOMAIN)
                .id(&key::builtin_derive_key(db, imp).0)
                .part(format!("{method:?}").as_bytes())
                .finish(),
        ),
    });
}
