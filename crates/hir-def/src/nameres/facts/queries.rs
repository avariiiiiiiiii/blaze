//! The salsa queries of the per-module resolver (`docs/v2/plan/lanes/frontend/nameres.md` §3,
//! `nameres-core.md` §3). Every key is content (crate, round, module, anchor module), never a
//! dense index, and every key's `Debug` names its module. Each query here is a thin entry; the
//! work is in `collect`, `unit`, `macros` and `driver`.

use base_db::{Db, Crate};
use hir_expand::{mod_path::ModPath, name::Name};
use rustc_hash::FxHashMap;
use triomphe::Arc;

use crate::{
    MacroId, ModuleId, UseId,
    item_tree::{ModItemId, TreeId},
    nameres::{DefMapCrateData, ModuleData, ModuleOrigin, mod_resolution::ModDir},
    per_ns::PerNs,
    visibility::Visibility,
};

use super::{
    collect, driver,
    env::{CollectEnv, CrateEnv, SideTables},
    graph::{ImportShape, Partition},
    macros,
    ops::{DeclOp, ImportOp, Inherited, MacroOp, OpLog},
    unit::{self, UnitState},
};

// Keys.

/// Which items a module's round-0 collection walks.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum DeclItems {
    /// The top-level items of the key's tree (a file module, the crate root).
    Top,
    /// The items of the inline `mod` item `ModItemId` of the key's tree.
    Inline(ModItemId),
    /// None: a module declared but not collected (its file not found, or past the `mod`
    /// depth limit). Its log is its `Init`.
    Nothing,
}

/// A module as it is declared at round 0: everything its collection reads besides item trees
/// and other crates.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Decl {
    pub(crate) krate: Crate,
    pub(crate) module: ModuleId,
    pub(crate) origin: ModuleOrigin,
    pub(crate) visibility: Visibility,
    pub(crate) parent: Option<ModuleId>,
    pub(crate) tree: TreeId,
    pub(crate) items: DeclItems,
    pub(crate) mod_dir: ModDir,
    pub(crate) macro_depth: u32,
    /// The parent's textual (`macro_rules!`) scope where the module is declared.
    pub(crate) inherited: Inherited,
    pub(crate) env: EnvKey,
}

#[salsa::interned(debug, unsafe(no_lifetime), revisions = usize::MAX)]
pub(crate) struct DeclKey {
    #[returns(ref)]
    pub(crate) decl: Decl,
}

#[salsa::interned(debug, unsafe(no_lifetime), revisions = usize::MAX)]
pub(crate) struct EnvKey {
    #[returns(ref)]
    pub(crate) env: CollectEnv,
}

#[salsa::interned(debug, unsafe(no_lifetime), revisions = usize::MAX)]
pub(crate) struct CrateEnvKey {
    #[returns(ref)]
    pub(crate) env: CrateEnv,
}

/// A round of a crate's collection.
#[salsa::interned(debug, unsafe(no_lifetime), revisions = usize::MAX)]
pub(crate) struct RoundKey {
    pub(crate) krate: Crate,
    pub(crate) round: u32,
}

/// A module at a round: `fact_handle`, `module_round`, `macro_step`, `unresolved_named`.
#[salsa::interned(debug, unsafe(no_lifetime), revisions = usize::MAX)]
pub(crate) struct FactKey {
    pub(crate) module: ModuleId,
    pub(crate) krate: Crate,
    pub(crate) round: u32,
}

/// A component of a round's import graph, by the smallest module index of its members (its
/// anchor, `Partition::anchor`), named by the module.
#[salsa::interned(debug, unsafe(no_lifetime), revisions = usize::MAX)]
pub(crate) struct SccKey {
    pub(crate) anchor: ModuleId,
    pub(crate) krate: Crate,
    pub(crate) round: u32,
}

/// Which modules a view reads through `row`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Rows {
    /// Every module of a round, each scope `ScopeRef::Fact` of that round.
    Round(RoundKey),
    /// What the round-0 collection of the module of `DeclKey` sees: its ancestors as its env
    /// holds them, every other module declared before it as its round-0 declarations.
    Decl(DeclKey),
}

#[salsa::interned(debug, unsafe(no_lifetime), revisions = usize::MAX)]
pub(crate) struct RowsKey {
    pub(crate) rows: Rows,
}

/// What a `ScopeSlot::Fact` holds.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Scope {
    /// The module's scope after round `FactKey::round`'s import fixed point.
    Fact(FactKey),
    /// The module's round-0 scope as its own collection left it.
    Decl(DeclKey),
}

#[salsa::interned(debug, unsafe(no_lifetime), revisions = usize::MAX)]
pub(crate) struct ScopeRef {
    pub(crate) scope: Scope,
}

/// What a round does after its import fixed point.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum RoundKind {
    Normal,
    /// Eager calls whose arguments name a macro that does not resolve are expanded with that
    /// error (the old `expand_eager_with_unresolved_args`).
    EagerWithErrors,
    /// The reseed of the first unresolved attribute: of `module`, at `index` of its pending
    /// directives (the old `reseed_with_unresolved_attribute`).
    Reseed { module: ModuleId, index: u32 },
    /// After the last round: units record their still-unresolved imports and the root of a
    /// proc-macro crate is censored (the old `collect`'s end); no macro step runs.
    Final,
}

// Values.

/// A module's round-0 collection.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct ModuleDecls {
    pub(crate) ops: OpLog,
    /// The children it declares, in declaration order; each has its own `module_decls`.
    pub(crate) children: Box<[DeclKey]>,
    /// Its textual scope after collection, every definition of each name.
    pub(crate) legacy: Inherited,
    /// Its tables after collection: where its first macro step starts.
    pub(crate) side: Arc<SideTables>,
}

/// The ops a module gains at a round `r >= 1`: from its own macro step of `r - 1`, the step that
/// declared it (its parent's), and, for the crate root, every step's root ops, in module order.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct ModuleRound {
    pub(crate) ops: OpLog,
}

/// One module of a round.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ModuleRow {
    pub(crate) origin: ModuleOrigin,
    pub(crate) visibility: Visibility,
    pub(crate) parent: Option<ModuleId>,
    /// The round it was declared at.
    pub(crate) since: u32,
    /// Its round-0 key, for one declared at round 0.
    pub(crate) decl: Option<DeclKey>,
}

/// Every module of a round, in the round's module order: round 0's DFS of `module_decls`,
/// then each later round's new modules in the order their steps declared them.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct RoundModules {
    pub(crate) order: Box<[ModuleId]>,
    pub(crate) rows: Box<[ModuleRow]>,
    pub(crate) index: FxHashMap<ModuleId, u32>,
}

impl RoundModules {
    pub(crate) fn row(&self, module: ModuleId) -> Option<&ModuleRow> {
        self.index.get(&module).map(|&at| &self.rows[at as usize])
    }
}

/// One component's resolution at one round: its members' scopes handed to the database, and
/// the state the next round carries on from.
#[derive(Debug)]
pub(crate) struct SccState {
    /// The round whose `ModuleScope`s these are: this one, or the last one at which the
    /// component changed (`latest`).
    pub(crate) round: u32,
    /// Members, ascending by module index of the round.
    pub(crate) members: Box<[ModuleId]>,
    /// Each member's `ModuleScope` (as `salsa::Id`), in `members` order.
    pub(crate) handles: Box<[salsa::Id]>,
    /// The `#[prelude_import]` this component resolved, if it holds it.
    pub(crate) prelude: Option<(ModuleId, Option<UseId>)>,
    /// Every module of another component the state read (a path's `PathRead::Slot`, a glob's
    /// source), with the `ModuleScope` it read, ascending by module: the next round keeps this
    /// state when its members gain no op and each of these is still the round's
    /// `fact_handle`.
    pub(crate) upstream: Box<[(ModuleId, salsa::Id)]>,
    /// At a `RoundKind::Final` round, every import of the members in import order with what it
    /// resolved to (`PartialResolvedImport::namespaces`; `None` while still unresolved), for
    /// `finish`'s diagnostics and `NameRecords`; empty at any other round.
    pub(crate) imports: Box<[(ImportOp, Option<PerNs>)]>,
    pub(crate) state: Arc<UnitState>,
}

/// Never equal to another execution's: a rerun is always a change to the next round, which
/// carries `state` on; `fact_handle` is what backdates.
impl PartialEq for SccState {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.state, &other.state) && self.handles == other.handles
    }
}
impl Eq for SccState {}

/// One module's macro resolution at one round.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct MacroStep {
    /// The ops of round `r + 1` it made, of any target (the module, the crate root, the
    /// modules it declared), in order.
    pub(crate) ops: OpLog,
    /// Directives left for the next round, in order.
    pub(crate) pending: Box<[MacroOp]>,
    /// The module's tables after the step.
    pub(crate) side: Arc<SideTables>,
    /// The modules it declared, in order, with their rows.
    pub(crate) new_modules: Box<[(ModuleId, ModuleRow)]>,
    /// Whether a call resolved (the old `resolve_macros` returning `No`).
    pub(crate) progress: bool,
    /// Whether an eager call was left for the error round.
    pub(crate) deferred_eager: bool,
}

/// The fold of a round's macro steps that picks the next round's kind.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct RoundOutcome {
    pub(crate) progress: bool,
    pub(crate) deferred_eager: bool,
    /// The first unresolved attribute directive, in module order then position.
    pub(crate) first_attr: Option<(ModuleId, u32)>,
}

// Queries.

/// The crate's root module id. Created here so every query of the crate shares it.
#[salsa::tracked]
pub(crate) fn crate_root(db: &Db, krate: Crate) -> ModuleId {
    // SAFETY: `to_static` needs the id not to outlive the query computation it was made in.
    // The id goes only into values computed from this query's (re-validated through salsa
    // when it reruns): the keys and logs of this crate's resolver queries and the `DefMap`
    // `crate_local_def_map` assembles from them. This query creates it with the same fields
    // every time, so the id is the same for as long as the crate is.
    unsafe {
        crate::ModuleIdLt::new(db, krate, None, None, Name::new_symbol_root(intern::sym::__empty))
            .to_static()
    }
}

/// The crate attributes of the root file (`seed_with_top_level`'s first part).
#[salsa::tracked(returns(ref))]
pub(crate) fn crate_data(db: &Db, krate: Crate) -> Arc<DefMapCrateData> {
    collect::crate_data(db, krate)
}

/// The crate root's round-0 key: the extern prelude of the crate's dependencies.
#[salsa::tracked]
pub(crate) fn root_decl(db: &Db, krate: Crate) -> DeclKey {
    collect::root_decl(db, krate)
}

/// One module's round-0 declarations.
#[salsa::tracked(returns(ref))]
pub(crate) fn module_decls(db: &Db, key: DeclKey) -> ModuleDecls {
    collect::module_decls(db, key)
}

/// The module's `macro_rules!` after collection, the last definition of each name: what a
/// `#[macro_use] mod` brings into its parent. Backdates unless they change.
#[salsa::tracked(returns(ref))]
pub(crate) fn legacy_exports(db: &Db, key: DeclKey) -> Box<[(Name, MacroId)]> {
    module_decls(db, key)
        .legacy
        .iter()
        .filter_map(|(name, macs)| Some((*name, *macs.last()?)))
        .collect()
}

/// The ops the module and its round-0 descendants make in the crate root (`#[macro_export]`),
/// in DFS order. Backdates unless they change.
#[salsa::tracked(returns(ref))]
pub(crate) fn subtree_macro_exports(db: &Db, key: DeclKey) -> Box<[DeclOp]> {
    let root = crate_root(db, key.decl(db).krate);
    let mut out = Vec::new();
    for op in module_decls(db, key).ops.iter() {
        match op {
            DeclOp::Splice { key, .. } => {
                out.extend(subtree_macro_exports(db, *key).iter().filter_map(|it| it.clone_root_op(root)));
            }
            op => out.extend(op.clone_root_op(root)),
        }
    }
    out.into_boxed_slice()
}

/// The key of `child`, a round-0 child of the module of `parent`.
#[salsa::tracked]
pub(crate) fn child_key(db: &Db, parent: DeclKey, child: ModuleId) -> Option<DeclKey> {
    module_decls(db, parent).children.iter().copied().find(|it| it.decl(db).module == child)
}

/// The module of `key`'s round-0 scope as a `ModuleScope`, for `Scope::Decl` reads.
#[salsa::tracked]
pub(crate) fn decl_handle(db: &Db, key: DeclKey) -> salsa::Id {
    let decl = key.decl(db);
    let scope = unit::local_scope(db, decl.module, &decl.inherited, &module_decls(db, key).ops);
    let scope = crate::nameres::ModuleScope::new(db, decl.module, scope.scope, scope.children);
    salsa::plumbing::AsId::as_id(&scope)
}

/// Every module of the round, in order.
#[salsa::tracked(returns(ref))]
pub(crate) fn round_modules(db: &Db, round: RoundKey) -> RoundModules {
    driver::round_modules(db, round)
}

/// What the module gains at a round `>= 1`.
#[salsa::tracked(returns(ref))]
pub(crate) fn module_round(db: &Db, key: FactKey) -> ModuleRound {
    driver::module_round(db, key)
}

/// The round's import graph, for `graph::partition`. Backdates unless an import, a `mod` or
/// an `extern crate self` changes.
#[salsa::tracked(returns(ref))]
pub(crate) fn import_shape(db: &Db, round: RoundKey) -> ImportShape {
    driver::import_shape(db, round)
}

#[salsa::tracked(returns(ref))]
pub(crate) fn partition(db: &Db, round: RoundKey) -> Partition {
    super::graph::partition(import_shape(db, round))
}

/// The crate env the round's units and steps start from: the root's crate-level ops of every
/// round before, and the injected std prelude.
#[salsa::tracked]
pub(crate) fn round_env(db: &Db, round: RoundKey) -> CrateEnvKey {
    driver::round_env(db, round)
}

/// One component's resolution at one round.
#[salsa::tracked(returns(ref))]
pub(crate) fn scc_state(db: &Db, key: SccKey) -> SccState {
    unit::scc_state(db, key)
}

/// The round whose state `scc_state(key)` hands out: `key`'s round, or the last before it at
/// which the component changed.
#[salsa::tracked]
pub(crate) fn latest(db: &Db, key: SccKey) -> u32 {
    scc_state(db, key).round
}

/// The module's `ModuleScope` after the round's import fixed point. Backdates while the
/// component hands out the same one.
#[salsa::tracked]
pub(crate) fn fact_handle(db: &Db, key: FactKey) -> salsa::Id {
    unit::fact_handle(db, key)
}

/// The module's plain imports still unresolved after the round's fixed point, by the name each
/// binds, in import order: what case 1 of a step's macro calls reads (`unresolved_named`).
#[salsa::tracked(returns(ref))]
pub(crate) fn unresolved_named(db: &Db, key: FactKey) -> Box<[(Name, ModPath)]> {
    unit::unresolved_named(db, key)
}

/// One module's macro resolution at one round.
#[salsa::tracked(returns(ref))]
pub(crate) fn macro_step(db: &Db, key: FactKey) -> MacroStep {
    macros::macro_step(db, key)
}

#[salsa::tracked]
pub(crate) fn round_progress(db: &Db, round: RoundKey) -> RoundOutcome {
    driver::round_progress(db, round)
}

#[salsa::tracked]
pub(crate) fn round_kind(db: &Db, round: RoundKey) -> RoundKind {
    driver::round_kind(db, round)
}

/// The row of `module` in a view that does not hold it.
#[salsa::tracked(returns(ref))]
pub(crate) fn row(db: &Db, rows: RowsKey, module: ModuleId) -> Option<ModuleData> {
    match rows.rows(db) {
        Rows::Round(round) => driver::round_row(db, round, module),
        Rows::Decl(key) => collect::decl_row(db, key, module),
    }
}

/// The `ModuleScope` (as `salsa::Id`) a `ScopeSlot::Fact` names.
pub(crate) fn scope_of(db: &Db, scope: ScopeRef) -> salsa::Id {
    match scope.scope(db) {
        Scope::Fact(key) => fact_handle(db, key),
        Scope::Decl(key) => decl_handle(db, key),
    }
}

/// A `ScopeSlot::Fact` for `scope`.
pub(super) fn fact_slot(db: &Db, scope: Scope) -> crate::nameres::ScopeSlot {
    crate::nameres::ScopeSlot::Fact(salsa::plumbing::AsId::as_id(&ScopeRef::new(db, scope)))
}
