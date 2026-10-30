//! Defines hir-level representation of visibility (e.g. `pub` and `pub(crate)`).

use std::iter;

use base_db::{Crate, SourceDatabase};
use hir_expand::{InFile, Lookup};
use la_arena::ArenaMap;
use syntax::ast::{self, HasVisibility};

use crate::{
    AssocItemId, BlockIdLt, HasModule, ItemContainerId, LocalFieldId, ModuleId, ModuleIdLt, TraitId,
    VariantId, nameres::DefMap, resolver::HasResolver, signatures::VariantFields, src::HasSource,
};

pub use crate::item_tree::{RawVisibility, VisibilityExplicitness};

/// Visibility of an item, with the path resolved.
#[derive(Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub enum Visibility {
    /// Visibility is restricted to a certain module.
    Module(ModuleId, VisibilityExplicitness),
    /// Visibility is restricted to the crate.
    PubCrate(Crate),
    /// Visibility is unrestricted.
    Public,
}

// A module id (8 bytes) and the explicitness, whose spare values tag the other two.
const _: () = assert!(size_of::<Visibility>() <= 12);

/// Whether `ancestor` is `module` or a module above it in their `DefMap`, by the parent each
/// module id carries.
fn is_ancestor_in_def_map<'db>(
    db: &'db dyn SourceDatabase,
    ancestor: ModuleIdLt<'db>,
    module: ModuleIdLt<'db>,
) -> bool {
    iter::successors(Some(module), |&m| m.containing_module_inside_def_map(db))
        .any(|m| m == ancestor)
}

impl Visibility {
    pub fn resolve(
        db: &dyn SourceDatabase,
        resolver: &crate::resolver::Resolver<'_>,
        raw_vis: &RawVisibility,
    ) -> Self {
        // we fall back to public visibility (i.e. fail open) if the path can't be resolved
        resolver.resolve_visibility(db, raw_vis).unwrap_or(Visibility::Public)
    }

    pub(crate) fn is_visible_from_other_crate(self) -> bool {
        matches!(self, Visibility::Public)
    }

    #[tracing::instrument(skip_all)]
    pub fn is_visible_from<'db>(
        self,
        db: &'db dyn SourceDatabase,
        from_module: ModuleIdLt<'db>,
    ) -> bool {
        let to_module = match self {
            // SAFETY: `to_db` needs `m` to be an id of `db`. A `Visibility` is resolved against
            // `db`'s def maps (`Visibility::resolve`, the collector) and asked only with the same
            // database, so `m` is one of `db`'s modules, and the `'db` copy is read only during
            // this call. A module of another database would name a slot of another table.
            Visibility::Module(m, _) => unsafe { m.to_db(db) },
            Visibility::PubCrate(krate) => return from_module.krate(db) == krate,
            Visibility::Public => return true,
        };
        if from_module == to_module {
            // if the modules are the same, visibility is trivially satisfied
            return true;
        }
        // if they're not in the same crate, it can't be visible
        if from_module.krate(db) != to_module.krate(db) {
            return false;
        }
        Self::is_visible_from_def_map_(db, from_module.block(db), to_module, from_module)
    }

    pub(crate) fn is_visible_from_def_map<'db>(
        self,
        db: &'db dyn SourceDatabase,
        def_map: &'db DefMap,
        from_module: ModuleIdLt<'db>,
    ) -> bool {
        if cfg!(debug_assertions) {
            _ = def_map.module(db, from_module);
        }
        let to_module = match self {
            Visibility::Module(m, _) => m,
            Visibility::PubCrate(krate) => return from_module.krate(db) == krate,
            Visibility::Public => return true,
        };
        if from_module == to_module {
            // if the modules are the same, visibility is trivially satisfied
            return true;
        }
        // if they're not in the same crate, it can't be visible
        if def_map.krate() != to_module.krate(db) {
            return false;
        }

        if from_module == to_module && def_map.block_id() == to_module.block(db) {
            // if the modules are the same, visibility is trivially satisfied
            return true;
        }
        Self::is_visible_from_def_map_(db, def_map.block_id(), to_module, from_module)
    }

    /// Whether `from_module`, in the `DefMap` of `def_map_block`, sees what is visible in
    /// `to_module`. Each step up reads the module id's own fields (its parent in its `DefMap`, or
    /// the module its block is in), never a `DefMap`: `to_module` may be in the one being
    /// computed.
    fn is_visible_from_def_map_<'db>(
        db: &'db dyn SourceDatabase,
        def_map_block: Option<BlockIdLt<'db>>,
        mut to_module: ModuleIdLt<'db>,
        mut from_module: ModuleIdLt<'db>,
    ) -> bool {
        // `to_module` might be the root module of a block expression. Those have the same
        // visibility as the containing module (even though no items are directly nameable from
        // there, getting this right is important for method resolution).
        // In that case, we adjust the visibility of `to_module` to point to the containing module.
        loop {
            match (to_module.block(db), def_map_block) {
                // `to_module` is not a block, so there is no parent def map to use.
                (None, _) => (),
                // `to_module` is at `def_map`'s block, no need to move further.
                (Some(a), Some(b)) if a == b => {}
                // The module the block is in is the parent of the block's `DefMap`.
                (Some(block), _) => {
                    to_module = block.module(db);
                    continue;
                }
            }
            break;
        }

        // from_module needs to be a descendant of to_module
        loop {
            if from_module == to_module {
                return true;
            }
            match from_module.containing_module(db) {
                Some(parent) => from_module = parent,
                // Reached the root module, nothing left to check.
                None => return false,
            }
        }
    }

    /// Returns the most permissive visibility of `self` and `other`.
    ///
    /// If there is no subset relation between `self` and `other`, returns `None` (i.e. they're only
    /// visible in unrelated modules).
    pub(crate) fn max(
        self,
        db: &dyn SourceDatabase,
        other: Visibility,
        def_map: &DefMap,
    ) -> Option<Visibility> {
        match (self, other) {
            (_, Visibility::Public) | (Visibility::Public, _) => Some(Visibility::Public),
            (Visibility::PubCrate(krate), Visibility::PubCrate(krateb)) => {
                if krate == krateb {
                    Some(Visibility::PubCrate(krate))
                } else {
                    None
                }
            }
            (Visibility::Module(mod_, _), Visibility::PubCrate(krate))
            | (Visibility::PubCrate(krate), Visibility::Module(mod_, _)) => {
                if mod_.krate(db) == krate { Some(Visibility::PubCrate(krate)) } else { None }
            }
            (Visibility::Module(mod_a, expl_a), Visibility::Module(mod_b, expl_b)) => {
                if mod_a == mod_b {
                    // Most module visibilities are `pub(self)`, and assuming no errors
                    // this will be the common and thus fast path.
                    return Some(Visibility::Module(
                        mod_a,
                        match (expl_a, expl_b) {
                            (VisibilityExplicitness::Explicit, _)
                            | (_, VisibilityExplicitness::Explicit) => {
                                VisibilityExplicitness::Explicit
                            }
                            _ => VisibilityExplicitness::Implicit,
                        },
                    ));
                }

                if mod_a.krate(db) != def_map.krate() || mod_b.krate(db) != def_map.krate() {
                    return None;
                }

                let def_block = def_map.block_id();
                if mod_a.block(db) != def_block || mod_b.block(db) != def_block {
                    return None;
                }

                if is_ancestor_in_def_map(db, mod_b, mod_a) {
                    // B is above A
                    return Some(Visibility::Module(mod_b, expl_b));
                }

                if is_ancestor_in_def_map(db, mod_a, mod_b) {
                    // A is above B
                    return Some(Visibility::Module(mod_a, expl_a));
                }

                None
            }
        }
    }

    /// Returns the least permissive visibility of `self` and `other`.
    ///
    /// If there is no subset relation between `self` and `other`, returns `None` (i.e. they're only
    /// visible in unrelated modules).
    pub(crate) fn min(
        self,
        db: &dyn SourceDatabase,
        other: Visibility,
        def_map: &DefMap,
    ) -> Option<Visibility> {
        match (self, other) {
            (vis, Visibility::Public) | (Visibility::Public, vis) => Some(vis),
            (Visibility::PubCrate(krate), Visibility::PubCrate(krateb)) => {
                if krate == krateb {
                    Some(Visibility::PubCrate(krate))
                } else {
                    None
                }
            }
            (Visibility::Module(mod_, exp), Visibility::PubCrate(krate))
            | (Visibility::PubCrate(krate), Visibility::Module(mod_, exp)) => {
                if mod_.krate(db) == krate { Some(Visibility::Module(mod_, exp)) } else { None }
            }
            (Visibility::Module(mod_a, expl_a), Visibility::Module(mod_b, expl_b)) => {
                if mod_a == mod_b {
                    // Most module visibilities are `pub(self)`, and assuming no errors
                    // this will be the common and thus fast path.
                    return Some(Visibility::Module(
                        mod_a,
                        match (expl_a, expl_b) {
                            (VisibilityExplicitness::Explicit, _)
                            | (_, VisibilityExplicitness::Explicit) => {
                                VisibilityExplicitness::Explicit
                            }
                            _ => VisibilityExplicitness::Implicit,
                        },
                    ));
                }

                if mod_a.krate(db) != def_map.krate() || mod_b.krate(db) != def_map.krate() {
                    return None;
                }

                let def_block = def_map.block_id();
                if mod_a.block(db) != def_block || mod_b.block(db) != def_block {
                    return None;
                }

                if is_ancestor_in_def_map(db, mod_b, mod_a) {
                    // B is above A
                    return Some(Visibility::Module(mod_a, expl_a));
                }

                if is_ancestor_in_def_map(db, mod_a, mod_b) {
                    // A is above B
                    return Some(Visibility::Module(mod_b, expl_b));
                }

                None
            }
        }
    }

    #[inline]
    pub fn is_public(&self) -> bool {
        matches!(self, Visibility::Public)
    }
}

#[salsa::tracked]
impl VariantFields {
    /// Resolve visibility of all specific fields of a struct or union variant.
    #[salsa::tracked(returns(ref))]
    pub fn field_visibilities(
        db: &dyn SourceDatabase,
        variant_id: VariantId,
    ) -> ArenaMap<LocalFieldId, Visibility> {
        let variant_fields = variant_id.fields(db);
        let fields = variant_fields.fields();
        if fields.is_empty() {
            return ArenaMap::default();
        }
        let resolver = variant_id.module(db).resolver(db);
        let mut res = ArenaMap::with_capacity(fields.len());
        for (field_id, field_data) in fields.iter() {
            res.insert(field_id, Visibility::resolve(db, &resolver, &field_data.visibility));
        }
        res
    }
}

pub fn visibility_from_ast(
    db: &dyn SourceDatabase,
    has_resolver: impl HasResolver + HasModule,
    ast_vis: InFile<Option<ast::Visibility>>,
) -> Visibility {
    let mut span_map = None;
    let raw_vis = crate::item_tree::visibility_from_ast(db, ast_vis.value, &mut |range| {
        span_map.get_or_insert_with(|| ast_vis.file_id.span_map(db)).span_for_range(range).ctx
    });
    match raw_vis {
        RawVisibility::PubSelf(explicitness) => {
            Visibility::Module(has_resolver.module(db), explicitness)
        }
        RawVisibility::PubCrate => Visibility::PubCrate(has_resolver.krate(db)),
        RawVisibility::Public => Visibility::Public,
        RawVisibility::Module(..) => Visibility::resolve(db, &has_resolver.resolver(db), &raw_vis),
    }
}

#[salsa::tracked]
impl AssocItemId {
    /// Resolve visibility of an assoc item.
    #[salsa::tracked(returns(copy))]
    pub fn assoc_visibility(self, db: &dyn SourceDatabase) -> Visibility {
        match self {
            AssocItemId::FunctionId(function_id) => {
                let loc = function_id.lookup(db);
                trait_item_visibility(db, loc.container).unwrap_or_else(|| {
                    let source = loc.source(db);
                    visibility_from_ast(db, function_id, source.map(|src| src.visibility()))
                })
            }
            AssocItemId::ConstId(const_id) => {
                let loc = const_id.lookup(db);
                trait_item_visibility(db, loc.container).unwrap_or_else(|| {
                    let source = loc.source(db);
                    visibility_from_ast(db, const_id, source.map(|src| src.visibility()))
                })
            }
            AssocItemId::TypeAliasId(type_alias_id) => {
                let loc = type_alias_id.lookup(db);
                trait_item_visibility(db, loc.container).unwrap_or_else(|| {
                    let source = loc.source(db);
                    visibility_from_ast(db, type_alias_id, source.map(|src| src.visibility()))
                })
            }
        }
    }
}

fn trait_item_visibility(
    db: &dyn SourceDatabase,
    container: ItemContainerId,
) -> Option<Visibility> {
    match container {
        ItemContainerId::TraitId(trait_) => Some(trait_visibility(db, trait_)),
        _ => None,
    }
}

fn trait_visibility(db: &dyn SourceDatabase, def: TraitId) -> Visibility {
    let loc = def.lookup(db);
    let source = loc.source(db);
    visibility_from_ast(db, def, source.map(|src| src.visibility()))
}
