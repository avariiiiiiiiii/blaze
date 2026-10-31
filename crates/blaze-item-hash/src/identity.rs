//! A body's identity (docs/v2/plan/design/item-hash.md, section 3): the a-priori hash its
//! inference, MIR and borrow check are filed under, the same for the same body in every
//! database, different for each change that can change what they compute.
//!
//! It hashes, each as its own part:
//!
//! - its crate's inputs (3.1, [`crate_inputs`]) and its crate's lang items by signature;
//! - its owner (3.2): the item it is the body of as "self", by signature and attributes, and
//!   the impl or trait that item is in by key and signature;
//! - its HIR (3.3) as [`hir_def::item_hash::portable_body`] writes it, anonymous, with no
//!   span, and what a location macro expanded to as a slot (`slots`);
//! - what each id the HIR names is (3.4, `entries`);
//! - what each path resolves to where it is written, and the traits in scope (3.5, `names`);
//! - the chain of modules it is in, by key (3.6): privacy and `super` paths.
//!
//! Not hashed: where in its file the body is, its text beyond its HIR, other bodies. What
//! inference reads beyond these is a fact (section 5) or refused (section 6).

mod entries;
mod inputs;
mod item;
pub(crate) mod names;
mod slots;

use base_db::Db;
use blaze_ids::{ContentId, Hasher};
use blaze_items::{ItemId, key};
use blaze_mem::scratch;
use hir_def::{
    AttrDefId, DefWithBodyId, GenericDefId, HasModule as _, ItemContainerId, Lookup as _,
    expr_store::Body, hir::generics::GenericParams, item_hash::portable_body,
};
use hir_expand::name::Name;
use smallvec::SmallVec;

pub use inputs::{FrontEnd, crate_inputs, set_build};

pub const IDENTITY_DOMAIN: &str = "blaze v2 item identity 2";

/// A body that has no identity, and why: its inference, MIR and borrow check are computed
/// and not filed.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub struct Unidentified {
    pub def: DefWithBodyId,
    pub what: &'static str,
}

/// The identity of `def`'s body.
pub fn identity(db: &Db, def: DefWithBodyId) -> Result<ContentId, Unidentified> {
    identity_of(db, def).map_err(|what| Unidentified { def, what })
}

#[salsa::tracked(returns(copy))]
fn identity_of(db: &Db, def: DefWithBodyId) -> Result<ContentId, &'static str> {
    if inputs::build(db).is_none() {
        return Err("no front-end build is set");
    }
    let home = def.module(db).krate(db);
    let mut h = Hasher::new(IDENTITY_DOMAIN);
    h.id(&crate_inputs(db, home));
    h.id(&inputs::lang_item_signatures(db, home)?);
    owner(db, def, &mut h)?;
    let names = names::names_of(db, def).as_ref().map_err(|it| *it)?;
    let portable =
        portable_body(Body::of(db, def), &names.blank, &slots::of(db, def), &lifetimes(db, def));
    h.part(portable.bytes());
    scratch::with_bytes(|buffer| -> Result<(), &'static str> {
        entries::write(db, home, item::own_item(def), &portable, buffer)?;
        h.part(buffer);
        Ok(())
    })?;
    h.part(&names.names).part(&names.traits);
    let mut module = Some(def.module(db));
    let mut depth = 0u32;
    while let Some(it) = module {
        h.id(&key::module_key(db, it).0);
        depth += 1;
        module = it.containing_module(db).map(|it| it.to_static());
    }
    h.update(&depth.to_le_bytes());
    Ok(h.finish())
}

/// The names of the lifetime parameters `def`'s body sees, its impl's or trait's first: a
/// lifetime of the body is hashed by its place among them, not its name.
fn lifetimes(db: &Db, def: DefWithBodyId) -> SmallVec<[Name; 4]> {
    let container = match def {
        DefWithBodyId::FunctionId(it) => Some(it.lookup(db).container),
        DefWithBodyId::ConstId(it) => Some(it.lookup(db).container),
        DefWithBodyId::StaticId(_) | DefWithBodyId::VariantId(_) => None,
    };
    let parent = match container {
        Some(ItemContainerId::ImplId(it)) => Some(GenericDefId::from(it)),
        Some(ItemContainerId::TraitId(it)) => Some(GenericDefId::from(it)),
        Some(ItemContainerId::ModuleId(_) | ItemContainerId::ExternBlockId(_)) | None => None,
    };
    // alloc: inline for a few, once per identity computed
    let mut names = SmallVec::new();
    for generic in parent.into_iter().chain([def.generic_def(db)]) {
        names.extend(GenericParams::of(db, generic).iter_lt().map(|(_, it)| it.name));
    }
    names
}

/// Section 3.2: the item `def` is the body of, as "self" by its signature and attributes (a
/// variant's discriminant: its enum by key and signature), and the impl or trait that item is
/// in by key and signature (its generics and where clauses are the body's too).
fn owner(db: &Db, def: DefWithBodyId, h: &mut Hasher) -> Result<(), &'static str> {
    let container = match def {
        DefWithBodyId::FunctionId(it) => {
            h.update(&[0]);
            Some(it.lookup(db).container)
        }
        DefWithBodyId::ConstId(it) => {
            h.update(&[1]);
            Some(it.lookup(db).container)
        }
        DefWithBodyId::StaticId(it) => {
            h.update(&[2]);
            Some(it.lookup(db).container)
        }
        DefWithBodyId::VariantId(it) => {
            h.update(&[3]).update(&item::variant_part(db, it)?);
            None
        }
    };
    match item::own_item(def) {
        Some(own) => {
            h.update(&item::self_part(db, own)?);
            h.id(&own_attrs(db, def).ok_or("its attributes have no hash")?);
        }
        None => {
            h.update(&item::item_part(db, ItemId::of_body(db, def))?);
        }
    }
    match container {
        Some(ItemContainerId::ImplId(it)) => {
            h.update(&[1]).update(&item::item_part(db, ItemId::Impl(it))?);
        }
        Some(ItemContainerId::TraitId(it)) => {
            h.update(&[2]).update(&item::item_part(db, ItemId::Trait(it))?);
        }
        Some(ItemContainerId::ModuleId(_) | ItemContainerId::ExternBlockId(_)) | None => {
            h.update(&[0]);
        }
    }
    Ok(())
}

/// The value of the `Attrs` fact of the item `def` is the body of, which [`owner`] hashes, so
/// that a read of it by the body's steps is covered; `None` for a variant's discriminant.
pub(crate) fn own_attrs(db: &Db, def: DefWithBodyId) -> Option<ContentId> {
    let owner = match def {
        DefWithBodyId::FunctionId(it) => AttrDefId::FunctionId(it),
        DefWithBodyId::ConstId(it) => AttrDefId::ConstId(it),
        DefWithBodyId::StaticId(it) => AttrDefId::StaticId(it),
        DefWithBodyId::VariantId(_) => return None,
    };
    crate::values::value(db, crate::values::attrs(db, owner)?)
}

/// Which reads of a [`covered_queries`] query the identity covers, by its key.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Key {
    /// Any key: name resolution, which the identity hashes as what each path of the body
    /// resolves to (3.5), its traits in scope and its module chain (3.6).
    Any,
    /// Only the body's own: its HIR, scopes and upvars are of its own bytes (3.3).
    OwnBody,
    /// Only the body's crate: its crate inputs (3.1).
    OwnCrate,
}

/// The front-end queries a body's identity stands for, each with the keys it covers: a read
/// of one of them by the body's inference, MIR building or borrow check with such a key is
/// covered (section 6) and records nothing. Lane item-hash-facts' `reads` holds each to the
/// same key.
pub fn covered_queries() -> &'static [(&'static str, Key)] {
    use Key::{Any, OwnBody, OwnCrate};
    &[
        // 3.3: the HIR is hashed as written; its source map is not (a read of it is refused).
        ("Body::of", OwnBody),
        // 3.3: the scopes are a function of the HIR.
        ("ExprScopes::body_expr_scopes", OwnBody),
        // 3.3: the closures and what they capture are of the HIR's expressions and bindings.
        // `closure_exprs` is keyed by an expression store: covered for the body's own.
        ("closure_exprs", OwnBody),
        ("body_upvars_mentioned", OwnBody),
        // 3.1: the crate's lang items by key and signature, and by key in the goal store's
        // crate key.
        ("lang_items", OwnCrate),
        ("crate_lang_items", OwnCrate),
        // 3.1: the `#![feature]` set.
        ("UnstableFeatures::query", OwnCrate),
        // 3.1: `recursion_limit`.
        ("crate_recursion_limit", OwnCrate),
        // 3.1: the crates loaded, in order, by key in the goal store's crate key.
        ("loaded_crates", OwnCrate),
        // 3.1: the goal store's crate key itself, and the crate key it holds.
        ("goal_crate_key", OwnCrate),
        ("crate_key", OwnCrate),
        // 3.1: the target, in the crate's workspace data.
        ("target_data_layout_query", OwnCrate),
        // 3.1: the `Crate` input's fields: the edition, origin, target kind and cfg (`data`,
        // `cfg_options`) and the name and version (`extra_data`) through the crate key, the
        // workspace data whole. Its dependencies are read through name resolution (3.5) and
        // `loaded_crates`, its root file and attributes through the HIR and the def map.
        ("data", OwnCrate),
        ("cfg_options", OwnCrate),
        ("extra_data", OwnCrate),
        ("workspace_data", OwnCrate),
        // 3.5 and 3.6: name resolution, the traits in scope, the module chain.
        ("crate_local_def_map", Any),
        ("block_def_map", Any),
        ("def_map", Any),
        ("DefMapPair", Any),
        ("crate_shadows_primitive_type", Any),
        ("scope", Any),
        ("children", Any),
        ("row", Any),
        ("module_trait_set", Any),
        ("ModuleScope", Any),
        ("ModuleScope::name", Any),
        ("ModuleScope::legacy_macro", Any),
        ("ModuleScope::traits", Any),
        ("Modules", Any),
        ("Modules::module", Any),
        ("Modules::name", Any),
        ("ModuleIdLt", Any),
        ("BlockIdLt", Any),
    ]
}
