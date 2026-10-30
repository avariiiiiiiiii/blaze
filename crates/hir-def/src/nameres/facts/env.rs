//! What every collection shares: the crate-level state it reads ([`CrateEnv`]), the
//! ancestors' state a round-0 collection reads ([`Ancestor`]), the per-module tables the macro
//! steps carry ([`SideTables`]), and the [`Sink`] each collection writes through.
//!
//! A collection is the old `ModCollector` over one of three sinks: a round-0 declaration view
//! (`collect::DeclSink`), a macro step's view (`macros::StepSink`), or, for a block def map, the
//! block's one [`super::unit::Unit`]. Each applies a mutation as the old `DefCollector` did to
//! the state it holds; the first two also record it as a [`DeclOp`].

use base_db::{Db, BuiltDependency};
use cfg::CfgOptions;
use hir_expand::{
    InFile, MacroCallId, attrs::AttrId, mod_path::ModPath, name::Name,
    proc_macro::CustomProcMacroExpander,
};
use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use syntax::ast;
use triomphe::Arc;

use crate::{
    AstId, ExternCrateId, FxIndexMap, MacroId, ModuleId, UseId,
    item_tree::ModItemId,
    nameres::{DefMap, DefMapCrateData, LocalDefMap, ModuleOrigin, mod_resolution::ModDir},
    per_ns::PerNs,
    visibility::Visibility,
};

use super::{
    ops::{DeclOp, DeferredDerive, DeriveHelper},
    queries::DeclKey,
};

/// The crate-level state paths read, as of one point of the crate's collection: the extern
/// prelude, the `macro_use` prelude and the std prelude. Only the root's collection changes the
/// first two, so nearly every module of a crate shares one (interned as
/// [`super::queries::CrateEnvKey`]). Lists keep insertion order, as the old maps did.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub(crate) struct CrateEnv {
    pub(crate) extern_prelude: Box<[(Name, ModuleId, Option<ExternCrateId>)]>,
    pub(crate) macro_use: Box<[(Name, MacroId, Option<ExternCrateId>)]>,
    pub(crate) prelude: Option<(ModuleId, Option<UseId>)>,
}

impl CrateEnv {
    /// The `LocalDefMap` paths resolve against.
    pub(crate) fn local_def_map(&self) -> LocalDefMap {
        let mut extern_prelude = FxIndexMap::default();
        extern_prelude.reserve(self.extern_prelude.len());
        for &(name, root, extern_crate) in &self.extern_prelude {
            extern_prelude.insert(name, (root, extern_crate));
        }
        LocalDefMap { extern_prelude }
    }

    /// Puts the `macro_use` and std preludes into `def_map`, which paths read there.
    pub(crate) fn install(&self, def_map: &mut DefMap) {
        def_map.macro_use_prelude.clear();
        def_map.macro_use_prelude.reserve(self.macro_use.len());
        for &(name, mac, extern_crate) in &self.macro_use {
            def_map.macro_use_prelude.insert(name, (mac, extern_crate));
        }
        def_map.prelude = self.prelude;
    }

    /// The env after `op`, for the crate-level ops; `None` when `op` leaves it as it is.
    pub(crate) fn after(&self, op: &DeclOp) -> Option<CrateEnv> {
        let mut next = match op {
            DeclOp::ExternPrelude { .. } | DeclOp::MacroUsePrelude { .. } | DeclOp::Prelude { .. } => {
                // clone: a crate-level op (the root's `extern crate`s, its prelude): a handful per crate
                self.clone()
            }
            _ => return None,
        };
        match *op {
            DeclOp::ExternPrelude { name, root, extern_crate } => {
                set_or_push(&mut next.extern_prelude, (name, root, extern_crate));
            }
            DeclOp::MacroUsePrelude { name, mac, extern_crate } => {
                set_or_push(&mut next.macro_use, (name, mac, extern_crate));
            }
            DeclOp::Prelude { module, import } => next.prelude = Some((module, import)),
            _ => unreachable!("matched above"),
        }
        Some(next)
    }
}

/// Replaces the entry of `entry.0` in `list`, or appends `entry`, keeping the list exact-size.
fn set_or_push<T: Copy>(list: &mut Box<[(Name, T, Option<ExternCrateId>)]>, entry: (Name, T, Option<ExternCrateId>)) {
    if let Some(it) = list.iter_mut().find(|it| it.0 == entry.0) {
        *it = entry;
        return;
    }
    // alloc: a crate-level op, a handful per crate.
    let mut grown = Vec::with_capacity(list.len() + 1);
    grown.extend_from_slice(list);
    grown.push(entry);
    *list = grown.into_boxed_slice();
}

/// An ancestor of a module, as it was when the chain toward the module was declared in it: what
/// a round-0 collection's paths (`pub(in path)`, case-1 macro paths into another crate) read
/// of it. Its type-namespace bindings that name a module, and its children so far with their
/// keys. A path that reads any other name of an ancestor at round 0 is one rustc rejects (a
/// visibility or macro path through a non-module), see the notes' §8.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct Ancestor {
    pub(crate) module: ModuleId,
    pub(crate) origin: ModuleOrigin,
    pub(crate) visibility: Visibility,
    pub(crate) parent: Option<ModuleId>,
    pub(crate) modules: Box<[(Name, PerNs)]>,
    pub(crate) children: Box<[(Name, ModuleId)]>,
    /// The round-0 keys of `children` that were collected (the chain child has none yet).
    pub(crate) child_keys: Box<[(ModuleId, DeclKey)]>,
}

/// What a round-0 collection of a module reads besides its own items: the crate env and its
/// ancestors, nearest first. Interned as [`super::queries::EnvKey`].
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub(crate) struct CollectEnv {
    pub(crate) krate: super::queries::CrateEnvKey,
    pub(crate) ancestors: Box<[Ancestor]>,
}

/// The constants of one crate's (or block's) collection: what the old `DefCollector` read from
/// its crate and never changed.
pub(crate) struct CollectCx<'db> {
    pub(crate) db: &'db Db,
    pub(crate) krate: base_db::Crate,
    pub(crate) data: Arc<DefMapCrateData>,
    pub(crate) deps: FxIndexMap<Name, BuiltDependency>,
    pub(crate) cfg_options: &'db CfgOptions,
    /// The crate's own procedural macros, from its built library (the old `proc_macros`).
    pub(crate) proc_macros: Box<[(Name, CustomProcMacroExpander, bool)]>,
    pub(crate) is_proc_macro: bool,
    /// `nameres::enable_builtin_derive_fast_path`, read once.
    pub(crate) builtin_derive_fast_path: bool,
    /// Whether this is a block's collection.
    pub(crate) in_block: bool,
}

/// One module's tables that collection and the macro steps read and write per item (the old
/// `DefCollector` held them crate-wide, keyed by item): carried from one step of the module to
/// the next.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct SideTables {
    /// The directory `mod` declarations of each module resolve in (`mod_dirs`).
    pub(crate) mod_dirs: FxHashMap<ModuleId, ModDir>,
    /// `skip_attrs`.
    pub(crate) skip_attrs: FxHashMap<InFile<ModItemId>, AttrId>,
    /// `prev_active_attrs`.
    pub(crate) prev_active_attrs: FxHashMap<AstId<ast::Item>, SmallVec<[AttrId; 1]>>,
    /// The module's part of `DefMap::derive_helpers_in_scope`.
    pub(crate) derive_helpers: FxHashMap<AstId<ast::Item>, Vec<DeriveHelper>>,
    /// `deferred_builtin_derives`.
    pub(crate) deferred_derives: FxHashMap<AstId<ast::Item>, Vec<DeferredDerive>>,
}

/// Where one collection's mutations go, and the state it reads while it collects.
///
/// `def_map()` is the view: the modules the sink holds (its own), every other module read
/// through [`super::queries::row`] (`ModulesSlot::Unit`), each of their scopes a fact.
pub(crate) trait Sink<'db> {
    fn db(&self) -> &'db Db;

    fn def_map(&self) -> &DefMap;

    /// The extern prelude paths read.
    fn local_def_map(&self) -> &LocalDefMap;

    /// Applies `op` to the held state as the old `DefCollector` did (an `Update` pushes, and,
    /// in a unit, wakes the imports that read the slot and propagates to glob importers), and
    /// records it where the sink keeps a log.
    fn apply(&mut self, op: DeclOp);

    /// The first `Some` of `f` over the paths of `module`'s still-unresolved plain imports that
    /// bind `name`, in import order (`Imports::unresolved_named`): the module's own ones
    /// collected so far, and in a step those its unit left unresolved.
    fn find_unresolved_named<T>(
        &self,
        module: ModuleId,
        name: Name,
        f: impl FnMut(&ModPath) -> Option<T>,
    ) -> Option<T>;

    /// Where a `mod` item of `parent` declares `child` (already pushed by `Init`/`PushChild`
    /// ops): `Separate(key)` when the child is its own `module_decls` query (round 0; the sink
    /// interns the key from `inherited` and its view, and holds the child's row with a
    /// declaration-view scope), `Inline` when the caller collects the child's items right here
    /// (a step, a block).
    fn child(&mut self, request: ChildRequest<'_>) -> ChildMode;

    /// Moves the macro directives of the sink's own module(s) that `apply(DeclOp::Macro)`
    /// received since the last call to the end of `into`, in order (the old
    /// `unresolved_macros.push`). A round-0 sink keeps them in its log instead and moves none.
    fn take_macros(&mut self, into: &mut Vec<super::ops::MacroOp>);

    /// The macro calls the sink's collections expanded at once (case 1) as `(module, call)`,
    /// for `expand_round`'s batch; a sink that does not batch ignores it.
    fn expanded_now(&mut self, module: ModuleId, call: MacroCallId) {
        let _ = (module, call);
    }
}

/// A child module a collection declares, for [`Sink::child`].
pub(crate) struct ChildRequest<'a> {
    pub(crate) parent: ModuleId,
    pub(crate) child: ModuleId,
    pub(crate) name: Name,
    pub(crate) origin: ModuleOrigin,
    pub(crate) visibility: Visibility,
    pub(crate) tree: crate::item_tree::TreeId,
    pub(crate) items: super::queries::DeclItems,
    pub(crate) mod_dir: ModDir,
    pub(crate) macro_depth: u32,
    /// The parent's textual scope at the declaration.
    pub(crate) inherited: &'a [(Name, Box<[MacroId]>)],
}

pub(crate) enum ChildMode {
    Separate(DeclKey),
    Inline,
}
