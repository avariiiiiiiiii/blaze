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

use crate::{
    ModuleId,
    item_tree::TreeId,
    nameres::{DefMap, LocalDefMap, ModuleData},
};

/// The crate's def map with every scope handed over (`ScopeSlot::Collected`), diagnostics
/// grouped: what `crate_local_def_map` wraps in a `DefMapPair`. Runs inside that query only.
pub(super) fn collect_crate(db: &Db, krate: Crate) -> (DefMap, LocalDefMap) {
    driver::collect_crate(db, krate)
}

/// A block's def map from the empty one `block_def_map` makes, scopes handed over.
pub(super) fn collect_block(
    db: &Db,
    def_map: DefMap,
    tree_id: TreeId,
    crate_local_def_map: &LocalDefMap,
) -> DefMap {
    driver::collect_block(db, def_map, tree_id, crate_local_def_map)
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
