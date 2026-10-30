//! Name resolution per module, with facts (`docs/v2/plan/lanes/frontend/nameres.md`).
//!
//! The old collector's rules, on a schedule that memoizes per module: each module's round-0
//! declarations are a query (`module_decls`) whose value is an op log; each expansion round
//! resolves imports per strongly connected component of the round's import graph
//! (`scc_state`), reading other components only through per-name facts (`ModuleScope::name`);
//! macro resolution is per module and round (`macro_step`); the crate's `DefMap` is assembled
//! from the final round's `ModuleScope` handles.

mod collect;
mod driver;
mod env;
mod graph;
pub(crate) mod impls;
pub mod kinds;
mod macros;
mod ops;
mod queries;
mod unit;
#[cfg(test)]
mod tests;

use base_db::{Db, Crate};
use blaze_mem::Frame;
use rustc_hash::FxBuildHasher;

use crate::{
    ModuleId,
    item_tree::TreeId,
    nameres::{DefMap, LocalDefMap, ModuleData},
};

/// What a query keeps while it runs, in the frame its `frame::task` opened: all of it goes when
/// the query returns, and what it returns is built from it at its exact size.
pub(crate) type FrameVec<'f, T> = Vec<T, &'f Frame>;

pub(crate) type FrameMap<'f, K, V> = hashbrown::HashMap<K, V, FxBuildHasher, &'f Frame>;

pub(crate) fn map_in<'f, K, V>(frame: &'f Frame) -> FrameMap<'f, K, V> {
    hashbrown::HashMap::with_hasher_in(FxBuildHasher, frame)
}

pub(crate) type FrameSet<'f, K> = hashbrown::HashSet<K, FxBuildHasher, &'f Frame>;

pub(crate) fn set_in<'f, K>(frame: &'f Frame) -> FrameSet<'f, K> {
    hashbrown::HashSet::with_hasher_in(FxBuildHasher, frame)
}

/// The crate's def map with every scope handed over (`ScopeSlot::Collected`), diagnostics
/// grouped: what `crate_local_def_map` wraps in a `DefMapPair`. Runs inside that query only, in
/// its frame.
pub(super) fn collect_crate(db: &Db, frame: &Frame, krate: Crate) -> (DefMap, LocalDefMap) {
    driver::collect_crate(db, frame, krate)
}

/// A block's def map from the empty one `block_def_map` makes, scopes handed over. Runs inside
/// that query only, in its frame.
pub(super) fn collect_block(
    db: &Db,
    frame: &Frame,
    def_map: DefMap,
    tree_id: TreeId,
    crate_local_def_map: &LocalDefMap,
) -> DefMap {
    driver::collect_block(db, frame, def_map, tree_id, crate_local_def_map)
}

/// The `ModuleScope` (as `salsa::Id`) of a `ScopeSlot::Fact(scope)`.
pub(super) fn fact_scope(db: &Db, scope: salsa::Id) -> salsa::Id {
    let scope = <queries::ScopeRef as salsa::plumbing::FromId>::from_id(scope);
    queries::scope_of(db, scope)
}

/// The row of `module` in a resolver view (`ModulesSlot::Unit`) that does not hold it.
pub(super) fn row(db: &Db, rows: salsa::Id, module: ModuleId) -> Option<&ModuleData> {
    let rows = <queries::RowsKey as salsa::plumbing::FromId>::from_id(rows);
    queries::row(db, rows, module).as_ref()
}
