//! The item key of a definition a stored record names, and back: what
//! `hir_ty::body_store` writes in place of each `SolverDefId` of an inference result, a MIR
//! body or a borrow check, so that a record written by one database is read by another.
//!
//! A key is [`blaze_items::key::def_key`]'s, like rustc's `DefPathHash`: the same definition
//! has the same key in every database. [`def_of`] goes back through
//! [`blaze_items::key::resolve`]. An anonymous constant's key resolves to its store and root
//! expression, but its id is interned with its type, which only the lowering of its owner
//! gives: `hir_ty::body_store` writes one by its location and type (`IdKind::AnonConst`), never
//! by key, so it and a closure written inside one (whose owner it is) give `None` here.

use base_db::Db;
use blaze_ids::ContentId;
use blaze_items::{
    ItemId, ItemKey,
    key::{self, Resolved},
};
use hir_def::{
    ExpressionStoreOwnerId,
    expr_store::{Body, ExpressionStore},
    hir::{ClosureKind, Expr, ExprId},
};
use hir_ty::{
    ImplTraitId, InferBodyId,
    db::{
        InternedClosure, InternedClosureId, InternedCoroutineClosureId, InternedCoroutineId,
        InternedOpaqueTyId,
    },
    next_solver::{ImplTraitIdx, SolverDefId},
};
use la_arena::RawIdx;

/// The key of `def`: the same definition has the same key in every database.
pub fn def_key<'db>(db: &'db Db, def: SolverDefId<'db>) -> ContentId {
    key::def_key(db, def).0
}

/// The definition of `db` whose [`def_key`] is `key`, `None` when it has none (or when it is
/// an anonymous constant's, see the module doc).
pub fn def_of<'db>(db: &'db Db, key: ContentId) -> Option<SolverDefId<'db>> {
    let def = match key::resolve(db, ItemKey(key))? {
        Resolved::Item(item) => item_def(item)?,
        Resolved::Module(_) => return None,
        Resolved::Variant(variant) => SolverDefId::EnumVariantId(variant),
        Resolved::BuiltinDerive(it) => SolverDefId::BuiltinDeriveImplId(it),
        Resolved::Ctor(it) => SolverDefId::Ctor(it),
        Resolved::AnonConst { .. } => return None,
        Resolved::Opaque { owner, index } => {
            let index = ImplTraitIdx::from_raw(RawIdx::from_u32(index));
            let loc = match owner {
                ItemId::Function(it) => ImplTraitId::ReturnTypeImplTrait(it, index),
                ItemId::Alias(it) => ImplTraitId::TypeAliasImplTrait(it, index),
                _ => return None,
            };
            SolverDefId::InternedOpaqueTyId(InternedOpaqueTyId::new(db, loc))
        }
        Resolved::Closure { store, expr, kind } => {
            let loc = InternedClosure { owner: closure_owner(db, store, expr)?, expr, kind };
            match kind {
                ClosureKind::Closure => {
                    SolverDefId::InternedClosureId(InternedClosureId::new(db, loc))
                }
                ClosureKind::OldCoroutine(_) | ClosureKind::Coroutine { .. } => {
                    SolverDefId::InternedCoroutineId(InternedCoroutineId::new(db, loc))
                }
                // Two keys resolve here: the coroutine closure's own and the closure's whose
                // type it has when called.
                ClosureKind::CoroutineClosure(_) => {
                    let it = InternedCoroutineClosureId::new(db, loc);
                    match key::coroutine_closure_key(db, it).0 == key {
                        true => SolverDefId::InternedCoroutineClosureId(it),
                        false => SolverDefId::InternedClosureId(it.as_closure(db)),
                    }
                }
            }
        }
    };
    debug_assert_eq!(def_key(db, def), key, "a key resolves to what has another key");
    Some(def)
}

fn item_def<'db>(item: ItemId) -> Option<SolverDefId<'db>> {
    Some(match item {
        ItemId::Function(it) => SolverDefId::FunctionId(it),
        ItemId::Adt(it) => SolverDefId::AdtId(it),
        ItemId::Trait(it) => SolverDefId::TraitId(it),
        ItemId::Impl(it) => SolverDefId::ImplId(it),
        ItemId::Const(it) => SolverDefId::ConstId(it),
        ItemId::Static(it) => SolverDefId::StaticId(it),
        ItemId::Alias(it) => SolverDefId::TypeAliasId(it),
        ItemId::Macro(_) => return None,
    })
}

/// What inferred the closure at `expr` of `store`: the body, for one in the body's own
/// expression or one of its `const { .. }` blocks (the body infers those); `None` for one in
/// another root, an anonymous constant that is inferred alone and is its owner.
fn closure_owner<'db>(
    db: &'db Db,
    store: ExpressionStoreOwnerId,
    expr: ExprId,
) -> Option<InferBodyId<'db>> {
    // A signature's or a variant's fields' closures are all in its anonymous constants.
    let ExpressionStoreOwnerId::Body(def) = store else { return None };
    let exprs = ExpressionStore::of(db, store);
    let root = exprs.find_root_for_expr(expr);
    let inferred_by_body = root == Body::of(db, def).root_expr()
        || exprs.exprs().any(|(_, it)| matches!(it, Expr::Const(inline) if inline == root));
    inferred_by_body.then_some(InferBodyId::DefWithBodyId(def))
}
