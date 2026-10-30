<!-- From mem-db-def-collect (child of mem-db-def). Notes: docs/v2/plan/lanes/mem/db-def-collect.md.
     Report: /tmp/blaze-lanes/v2-mem-db-def-collect.md. Line numbers are as read on 2026-10-29;
     the files move under other lanes, so the function named with each line is what to match. -->

# mem-db-def-collect: def-map collection on the query frame

Contract: `docs/v2/plan/lanes/mem/db.md` (points 1 to 7). Every collecting query opens the frame
salsa's `execute` made for its run (`blaze_mem::frame::current`, requested from blaze-mem by
mem-db), threads `&'f Frame` into its collector, and keeps every per-run temporary there. What it
returns is owned, on the global allocator, at exact size, made once from the frame vectors.
Manifests and `#![feature(allocator_api)]` are mem-db-def's H1 to H3 (not refiled here).

Owners: `nameres.rs`, `nameres/collector.rs`, `assoc.rs`, `diagnostics.rs`, `demand.rs`,
`frozen.rs`, `uses.rs`, `mod_resolution.rs`, `proc_macro.rs`, `attr_resolution.rs` and the new
test: **db-expand-names** (window `db-expand-names-next1`). `nameres/facts.rs` and `nameres/facts/`:
**frontend-nameres** (window `frontend-nameres-next1`, children
`frontend-nameres-core-{collect,unit,macros,driver}`). Part E is for mem-db-def-resolve and
mem-db-def-expand.

Rules every change below keeps:

- `frame::current` only in a query function's body. Code reached outside query runs (IDE,
  `Resolver`, `resolve_path_fp*`, `demand::walk_bodies` from drivers) keeps its signature and
  uses `blaze_mem::scratch` (Part D3).
- A closure given to `Db::run_all` (`F: Fn(&Db, usize) + Sync`) captures `&[T]`, never a
  `&Vec<T, &Frame>`: `Frame` is not `Sync`.
- Frame vectors drop their elements when dropped (a `Name` holds a `Symbol`); every frame
  collection here lives inside the `current` closure, so it is dropped before the task ends.
- `Default` is not implemented for `Vec<T, &Frame>`: every `mem::take` of a frame vector
  becomes `mem::replace(&mut v, Vec::new_in(frame))` (allocates nothing).
- Out of frame room is blaze-mem's `MemError` (the frame's own reserve-or-stop); nothing here
  falls back to the global heap.

## Tags

| Query (file) | Tag |
|---|---|
| `crate_local_def_map`, `block_def_map` (`nameres.rs`) | `Tag::Nameres` |
| `Modules::module`, `ModuleScope::{name, legacy_macro, traits}` (`nameres.rs`) | `Tag::Nameres` |
| `TraitItems::query_with_diagnostics`, `ImplItems::of` (`assoc.rs`) | `Tag::Nameres` |
| `used_imports` (`uses.rs`) | `Tag::Nameres` |
| `macros_declaring_roots`, `macros_writing_diagnostic_items` (`demand.rs`) | `Tag::Nameres` |
| every `#[salsa::tracked]` fn in `facts/queries.rs`, `facts/driver.rs`, `facts/macros.rs`, `facts/impls.rs` (`module_decls`, `legacy_exports`, `subtree_macro_exports`, `round_modules`, `module_round`, `import_shape`, `partition`, `round_env`, `scc_state`, `fact_handle`, `unresolved_named`, `macro_step`, `round_progress`, `round_kind`, `round_row`, `module_impls`, ...) | `Tag::Nameres` |

Written with mem-db-salsa's tag attribute; until it lands, `Tag::Nameres` is what each frame is
re-tagged to (`frame.set_tag(Tag::Nameres)` as the first line inside `current`, if blaze-mem
exposes it; otherwise the attribute alone).

Shared aliases, added at the top of `nameres.rs` (used by `collector.rs`, `facts/`, `assoc.rs`,
`uses.rs`, `demand.rs`, `frozen.rs`, and requested of `item_scope.rs` in Part E):

```rust
use blaze_mem::Frame;
use rustc_hash::FxBuildHasher;

/// A per-run vector, in the query's frame.
pub(crate) type FrameVec<'f, T> = Vec<T, &'f Frame>;
/// A per-run map, in the query's frame.
pub(crate) type FrameMap<'f, K, V> = hashbrown::HashMap<K, V, FxBuildHasher, &'f Frame>;
/// A per-run set, in the query's frame.
pub(crate) type FrameSet<'f, T> = hashbrown::HashSet<T, FxBuildHasher, &'f Frame>;

pub(crate) fn frame_map<'f, K, V>(frame: &'f Frame) -> FrameMap<'f, K, V> {
    hashbrown::HashMap::with_hasher_in(FxBuildHasher, frame)
}
pub(crate) fn frame_set<'f, T>(frame: &'f Frame) -> FrameSet<'f, T> {
    hashbrown::HashSet::with_hasher_in(FxBuildHasher, frame)
}
```

`hashbrown`'s `Entry` has the same `Occupied`/`Vacant`/`or_insert`/`or_insert_with` as std's;
`.entry(k).or_default()` on a map whose values are frame vectors becomes
`.entry(k).or_insert_with(|| Vec::new_in(frame))`.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

## Part A. `nameres.rs`

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### A1. `crate_local_def_map` (line 552)

Before (body after the tracing span):

```rust
    if let Some((def_map, local_def_map)) = frozen::served(db, crate_id) {
        return DefMapPair::new(db, def_map, local_def_map);
    }
    ...
    let (mut def_map, local_def_map) =
        collector::collect_defs(db, def_map, TreeId::new(root_file_id.into(), None), None);
    def_map.hand_over_scopes(db);
    frozen::check(db, crate_id, &def_map, &local_def_map);

    DefMapPair::new(db, def_map, local_def_map)
```

After: the whole body, span included, inside the frame:

```rust
#[salsa::tracked(returns(ref))]
pub(crate) fn crate_local_def_map(db: &Db, crate_id: Crate) -> DefMapPair<'_> {
    blaze_mem::frame::current(|frame| {
        let krate = crate_id.data(db);
        let _p = /* the span, unchanged */;
        if let Some((def_map, local_def_map)) = frozen::served_in(db, crate_id, frame) {
            return DefMapPair::new(db, def_map, local_def_map);
        }
        let root_file_id = crate_id.root_file_id(db);
        let module_data = /* unchanged */;
        let def_map = DefMap::empty(/* unchanged */);
        let (mut def_map, local_def_map) = collector::collect_defs(
            db,
            frame,
            def_map,
            TreeId::new(root_file_id.into(), None),
            None,
        );
        def_map.hand_over_scopes(db);
        frozen::check(db, crate_id, &def_map, &local_def_map);
        DefMapPair::new(db, def_map, local_def_map)
    })
}
```

When the facts driver serves crates (`facts::collect_crate`), its call takes `frame` the same way:
`facts::collect_crate(db, frame, crate_id)`.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### A2. `block_def_map` (line 591)

The body inside `blaze_mem::frame::current(|frame| { ... })`; the collect call becomes
`collector::collect_defs(db, frame, def_map, TreeId::new(ast_id.file_id, Some(block_id)), Some(local_def_map.local(db)))`
(and `facts::collect_block(db, frame, def_map, tree_id, local)` under the facts engine). The
`unsafe { to_static() }` blocks and their SAFETY comments stay as they are, inside the closure.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### A3. `DefMap.diagnostics` (line 244) and `derive_helpers_in_scope` (line 234)

| Field | Before | After |
|---|---|---|
| `diagnostics` | `Vec<DefDiagnostic>`, pushed by the collector (13 sites), then `shrink_to_fit` | `Box<[DefDiagnostic]>`, made once in `collector::finish` from the collector's frame vector (B4) |
| `derive_helpers_in_scope` | `FxHashMap<AstId<ast::Item>, Vec<DeriveHelper>>`, grown during collection, `shrink_to_fit` | `FxHashMap<AstId<ast::Item>, Box<[DeriveHelper]>>`, built once at exact capacity from the collector's frame map (B4) |

with `type DeriveHelper = (Name, MacroId, Either<MacroCallId, BuiltinDeriveImplId>);` named in
`nameres.rs`. Only the collector reads `derive_helpers_in_scope` while a map is collected
(`collector.rs` 2265, 2312, 2498, 2716, 2721, 2983); `path_resolution.rs` does not. So it is staged in
`DefCollector` (B1) and published at `finish`. `DefMap::derive_helpers_in_scope()` (line 787) keeps
its signature (`Box<[T]>` derefs to `[T]` as `Vec` did). `DefMap::empty` sets
`diagnostics: Box::default()` and `derive_helpers_in_scope: FxHashMap::default()` (neither
allocates).

`DefMap` gains one crate-visible setter used by `finish`, the facts driver and `frozen`:

```rust
    /// The def map's diagnostics and derive helpers, once, from the collection's frame.
    pub(super) fn publish<'f>(
        &mut self,
        diagnostics: FrameVec<'f, DefDiagnostic>,
        helpers: FrameMap<'f, AstId<ast::Item>, FrameVec<'f, DeriveHelper>>,
    ) {
        always!(self.diagnostics.is_empty() && self.derive_helpers_in_scope.is_empty());
        // One allocation: `Drain` is `TrustedLen`, so the vector is made at its length.
        self.diagnostics = diagnostics.into_iter().collect();
        let mut map = FxHashMap::with_capacity_and_hasher(helpers.len(), Default::default());
        for (item, list) in helpers {
            map.insert(item, Box::from(&list[..]));
        }
        self.derive_helpers_in_scope = map;
    }
```

(`DeriveHelper` is `Copy`: `Name` and the ids are. `vec::IntoIter<T, &Frame>` is `TrustedLen`, so
`collect::<Box<[T]>>()` allocates exactly once.)

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### A4. `DefMap::shrink_to_fit` (line 672) becomes `DefMap::settle`

Before: `macro_use_prelude.shrink_to_fit()`, `diagnostics.shrink_to_fit()`,
`derive_helpers_in_scope.shrink_to_fit()`, `modules.shrink_to_fit()`, and per module
`children_mut().shrink_to_fit()`, `settle_glob_ambiguities()`, `scope_mut().shrink_to_fit()`.

After: `diagnostics` and `derive_helpers_in_scope` are exact already (A3), so they leave the list.
What path resolution reads *while* the map is collected stays in the map and grows there:
`macro_use_prelude`, the `ModulesMap`, each module's `ItemScope` and `children`. Those are
rebuilt at exact size once, in place of the realloc `shrink_to_fit` made:

```rust
    /// Once collection is done: every table read while collecting, made again at its size.
    fn settle(&mut self) {
        let Self { macro_use_prelude, modules, .. } = self;
        *macro_use_prelude = exact_map(mem::take(macro_use_prelude));
        if let ModulesSlot::Building(modules) = modules {
            modules.settle();
            for (_, module) in modules.iter_mut() {
                let children = module.children_mut();
                *children = mem::take(children).into_iter().collect_exact();
                module.scope_mut().settle_glob_ambiguities();
                module.scope_mut().settle();
            }
        }
    }
```

with, in `nameres.rs`:

```rust
/// `map` again at exactly its length: one allocation, the same count `shrink_to_fit` made, and
/// no slack kept.
fn exact_map<K: Hash + Eq, V>(map: FxHashMap<K, V>) -> FxHashMap<K, V> {
    let mut out = FxHashMap::with_capacity_and_hasher(map.len(), Default::default());
    out.extend(map);
    out
}
```

`ModulesMap::settle` and `collect_exact` for `FxIndexMap` do the same (`with_capacity_and_hasher(len)`
then `extend`). `ItemScope::settle` is Part E1. The name is changed so no caller keeps reading it
as a shrink. `collect_defs` (line 136) and `facts/driver.rs` (557, 766) call `settle`.

The growth of these four tables during the miss stays on the global heap: they are part of the
`DefMap` that `resolve_path_fp` reads, and the result cannot borrow the frame. The notes give the
long-term move (staging them in the frame behind the resolve-side view). Under the facts engine the
crate `DefMap` is assembled from final rows (`driver.rs` 445: `ModulesMap` reserved to
`modules.order.len()`), so only blocks and the old collector still grow them.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### A5. `LocalDefMap::shrink_to_fit` and `DefMapCrateData::shrink_to_fit` (line 327)

`LocalDefMap.extern_prelude: FxIndexMap` is filled by the collector one dependency at a time.
It becomes built once from the collector's frame list in `finish` (B4):
`FxIndexMap::with_capacity_and_hasher(n, ..)` plus `extend`. `LocalDefMap::shrink_to_fit` goes.
`DefMapCrateData::shrink_to_fit` becomes `settle`, with `exact_map` for the three maps.
`registered_tools: Vec<Symbol>` becomes `Box<[Symbol]>`, made once after the crate attributes are
read: the collector gathers `#![register_tool]` names in a frame vector, then one
`Vec::with_capacity(PREDEFINED_TOOLS.len() + extra)` is filled and boxed (exact, no realloc).
`unstable_features.shrink_to_fit()` is `UnstableFeatures`' own (hir-def items slice); it stays
until that type is built at size.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### A6. `DiagnosticsByModule::of` (line 262)

`order` is exact (`(0..len).collect()` from a `Range`). `modules` grows by `push` and is shrunk by
`.into()`. After: count the runs first, then fill at exact size, with no frame needed:

```rust
    fn of(diagnostics: &[DefDiagnostic]) -> DiagnosticsByModule {
        let len = u32::try_from(diagnostics.len()).expect("a def map has 2^32 diagnostics");
        let mut order: Vec<u32> = (0..len).collect();
        order.sort_by_key(|&at| diagnostics[at as usize].in_module);
        let module_of = |at: &u32| diagnostics[*at as usize].in_module;
        let runs = order.chunk_by(|a, b| module_of(a) == module_of(b)).count();
        let mut modules = Vec::with_capacity(runs);
        let mut start = 0u32;
        for run in order.chunk_by(|a, b| module_of(a) == module_of(b)) {
            modules.push((module_of(&run[0]), start));
            start += run.len() as u32;
        }
        DiagnosticsByModule { order: order.into_boxed_slice(), modules: modules.into_boxed_slice() }
    }
```

`sort_by_key` is a stable sort and allocates scratch for long slices. It becomes
`order.sort_unstable_by_key(|&at| (diagnostics[at as usize].in_module, at))`: same order, since the
index breaks ties, and no allocation.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### A7. `ModuleScope::traits` (line 514)

Before: `self.scope(db).traits().collect()`. `traits()` is a `filter`, so the box grows and is
shrunk. After:

```rust
    #[salsa::tracked(returns(ref))]
    fn traits(self, db: &'db Db) -> Box<[crate::TraitId]> {
        let scope = self.scope(db);
        let mut out = Vec::with_capacity(scope.traits().count());
        out.extend(scope.traits());
        out.into_boxed_slice()
    }
```

`legacy_macro` (`Box::from(&[MacroId])`) and `name` (`PerNs`, no box) are exact already.
`Modules::module` clones a `ModuleData` (no box: `ScopeSlot::Collected` is an id).

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### A8. `ModuleData::new` (line 1113) and `DefMap::empty` (line 639)

`ScopeSlot::Building(Box::default())` is one global box per module, dropped at `hand_over_scopes`.
That stays: the `Box<BuildingScope>` is how a collected `ModuleData` keeps 16 bytes. The note in
A4 covers it as part of the building `DefMap`. `DefMap::empty`'s `ModulesMap::new()` stays
(no allocation until the root is inserted).

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

## Part B. `nameres/collector.rs`

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### B1. `collect_defs` (line 72) and `DefCollector` (line 745)

Signature: `pub(super) fn collect_defs<'f>(db: &Db, frame: &'f Frame, def_map: DefMap, tree_id: TreeId, crate_local_def_map: Option<&LocalDefMap>) -> (DefMap, LocalDefMap)`.
`DefCollector<'db>` becomes `DefCollector<'db, 'f>` with a new field `frame: &'f Frame`;
`ModCollector<'a, 'db>` (line 2928) becomes `ModCollector<'a, 'db, 'f>` holding
`def_collector: &'a mut DefCollector<'db, 'f>`; every `impl` and every closure typed
`&mut DefCollector<'_>` (e.g. `update_def`) gains the `'_`.

| Line | Field / local | Before | After |
|---|---|---|---|
| 82, 752 | `deps` | `FxIndexMap<Name, BuiltDependency>` (`dep.clone()` each) | `FrameVec<'f, (Name, &'db BuiltDependency)>`: insert replaces in place when the name is there (`iter_mut().find`), else pushes (the `FxIndexMap::insert` order); `get(name)` is `iter().find` (tens of entries) |
| 90 | `proc_macros` | `Box<[(Name, CustomProcMacroExpander, bool)]>` from `ProcMacros::list` | unchanged here; Part E3 asks hir-expand for `list_in(ctx, frame)` |
| 106, 753 | `globs` | `GlobEdges` = `ImporterLists<ModuleId, (Visibility, GlobId)>` | `GlobEdges<'f>`, B3 |
| 107, 754 | `imports` | `Imports<'db>` | `Imports<'db, 'f>`, B2 |
| 108, 757 | `held` | `Vec<(Option<Name>, PerNs)>` | `FrameVec<'f, (Option<Name>, PerNs)>` |
| 109, 759 | `path_buf` | `String` | removed: B5 |
| 110, 761 | `round_resolved` | `Vec<ResolvedCall>` | `FrameVec<'f, ResolvedCall>` |
| 111, 762 | `round_calls` | `Vec<MacroCallId>` | `FrameVec<'f, MacroCallId>` |
| 112, 763 | `macros_spare` | `Vec<MacroDirective<'db>>` | `FrameVec<'f, MacroDirective<'db>>` |
| 113, 764 | `unresolved_macros` | `Vec<MacroDirective<'db>>` | `FrameVec<'f, MacroDirective<'db>>` |
| 116, 775 | `mod_dirs` | `FxHashMap<ModuleId, ModDir>` | `FrameMap<'f, ModuleId, ModDir>` |
| 119, 783 | `from_glob_import` | `PerNsGlobImports` (three `FxHashSet<(ModuleId, Name)>`) | `PerNsGlobImports<&'f Frame>`, Part E1 |
| 120, 791 | `skip_attrs` | `FxHashMap<InFile<ModItemId>, AttrId>` | `FrameMap<'f, InFile<ModItemId>, AttrId>` |
| 121, 795 | `prev_active_attrs` | `FxHashMap<AstId<ast::Item>, SmallVec<[AttrId; 1]>>` (spills to the heap past one) | `FrameMap<'f, AstId<ast::Item>, FrameVec<'f, AttrId>>`; line 2393 `entry(ast_id).or_insert_with(|| Vec::new_in(frame))` |
| 122, 774 | `unresolved_extern_crates` | `FxHashSet<Name>` | `FrameSet<'f, Name>` |
| 124, 800 | `deferred_builtin_derives` | `FxHashMap<AstId<ast::Item>, Vec<DeferredBuiltinDerive>>` | `FrameMap<'f, AstId<ast::Item>, FrameVec<'f, DeferredBuiltinDerive>>`; the `consider_deferred_derives` closure parameter type follows |
| 125 | `resolved_macro_paths` | `Vec<ResolvedMacroPath>` | `FrameVec<'f, ResolvedMacroPath>` |
| 126, 803 | `derive_helper_uses` | `Vec<DeriveHelperUse>` | `FrameVec<'f, DeriveHelperUse>` |
| 127, 805 | `not_introducing` | `Vec<ExternCrateId>` | `FrameVec<'f, ExternCrateId>` |
| new | `diagnostics` | (pushed into `self.def_map.diagnostics`) | `FrameVec<'f, DefDiagnostic>`; every `self.def_map.diagnostics.push(..)` (913, 1062, 2424, 2562, 2575, 2777, 2795, 2830, 3128, 3464, 3972 and the rest `rg 'def_map.diagnostics.push'` finds) becomes `self.diagnostics.push(..)` / `self.def_collector.diagnostics.push(..)` |
| new | `derive_helpers` | (in `self.def_map.derive_helpers_in_scope`) | `FrameMap<'f, AstId<ast::Item>, FrameVec<'f, DeriveHelper>>`; the six reads/writes (2265, 2312, 2498, 2716, 2721, 2983) go to it, `or_default()` becoming `or_insert_with(|| Vec::new_in(frame))` |
| new | `extern_prelude` | (inserted into `self.local_def_map.extern_prelude`) | `FrameVec<'f, (Name, (CrateRootModuleId, Option<ExternCrateId>))>`, with the map's replace-in-place rule, published in `finish` (A5) |

`LocalDefMap` and the `DefMap` themselves stay owned (they are the result).

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### B2. `Imports<'db>` (line 299) becomes `Imports<'db, 'f>`

`fn new() -> Self` (line 339) becomes `fn new_in(frame: &'f Frame) -> Self`, and each field
changes as follows:

| Field | Before | After |
|---|---|---|
| `directives` | `Vec<ImportDirective<'db>>` | `FrameVec<'f, ImportDirective<'db>>` |
| `list` | `Vec<ImportList>` | `FrameVec<'f, ImportList>` |
| `generation` | `Vec<u32>` | `FrameVec<'f, u32>` |
| `queued` | `Vec<u8>` | `FrameVec<'f, u8>` |
| `seq` | `Vec<u32>` | `FrameVec<'f, u32>` |
| `indeterminate` | `Vec<(u32, PerNs)>` | `FrameVec<'f, (u32, PerNs)>` |
| `named` | `FxHashMap<(ModuleId, Name), (u32, u32)>` | `FrameMap<'f, (ModuleId, Name), (u32, u32)>` |
| `next_named` | `Vec<u32>` | `FrameVec<'f, u32>` |
| `pending_unresolved`, `pending_indeterminate` | `BinaryHeap<Reverse<u32>>` | `BinaryHeap<Reverse<u32>, &'f Frame>` (`BinaryHeap::new_in(frame)`) |
| `deferred_unresolved`, `deferred_indeterminate` | `Vec<u32>` | `FrameVec<'f, u32>` |
| `waits` | `FxHashMap<WaitOn, u32>` | `FrameMap<'f, WaitOn, u32>` |
| `edges` | `Vec<WaitEdge>` | `FrameVec<'f, WaitEdge>` |
| `any_waiters` | `Vec<(u32, u32)>` | `FrameVec<'f, (u32, u32)>` |
| `keys` | `Vec<(WaitOn, u8)>` | `FrameVec<'f, (WaitOn, u8)>` |
| `woken` | `Vec<u32>` | `FrameVec<'f, u32>` |

`#[cfg(test)] rounds` stays (test-only). `wake_any` (line 578) uses `mem::take` on a frame
vector. It becomes an index walk (`wake` never adds to `any_waiters`):

```rust
    fn wake_any(&mut self) {
        for at in 0..self.any_waiters.len() {
            let (import, generation) = self.any_waiters[at];
            if self.generation[import as usize] == generation {
                self.wake(import);
            }
        }
        self.any_waiters.clear();
    }
```

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### B3. `ImporterLists<K, T>` (line 630) becomes `ImporterLists<'f, K, T>`

`edges: FrameVec<'f, ImporterEdge<K, T>>`, `heads: FrameMap<'f, K, (u32, u32)>`,
`by_pair: FrameMap<'f, (K, K), u32>`; `new()` (639) becomes `new_in(frame)`;
`type GlobEdges<'f> = ImporterLists<'f, ModuleId, (Visibility, GlobId)>;` (682).

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### B4. `finish` (line 2745) and `name_records` (line 2850)

`finish` publishes what the frame staged. It replaces the `def_map.shrink_to_fit()` and
`local_def_map.shrink_to_fit()` in `collect_defs` (136, 137):

```rust
        self.def_map.names = self.name_records();
        let diagnostics = mem::replace(&mut self.diagnostics, Vec::new_in(self.frame));
        let helpers = mem::replace(&mut self.derive_helpers, frame_map(self.frame));
        self.def_map.publish(diagnostics, helpers);
        self.local_def_map.extern_prelude = {
            let mut map = FxIndexMap::with_capacity_and_hasher(self.extern_prelude.len(), Default::default());
            map.extend(self.extern_prelude.drain(..));
            map
        };
        (self.def_map, self.local_def_map)
```

and `collect_defs` ends `let (mut def_map, local_def_map) = collector.finish(); def_map.settle(); (def_map, local_def_map)`.

`name_records`: `uses`, `found`, `unresolved`, `glob_targets` and `macro_calls` (2855 to 2888)
become `Vec::new_in(self.frame)`, and `seen` (2914) becomes `frame_set(self.frame)`. Each box is
made at its length: `uses: Box::from(&uses[..])` (`Use` is `Copy`, as the `retain(|&it| ..)` reads
it), `unresolved: Box::from(&unresolved[..])`, `glob_targets: Box::from(&glob_targets[..])`,
`macro_calls: Box::from(&macro_calls[..])`, `derive_helpers: Box::from(&self.derive_helper_uses[..])`
and `not_introducing: Box::from(&self.not_introducing[..])`, replacing the
`into_boxed_slice` (a realloc whenever a `Vec` has slack). `path_uses` and
`path_uses_outside_imports` take `out: &mut Vec<Use>`. They become generic, `out: &mut Vec<Use, A>`
with `A: Allocator`, in `uses.rs` (D2).

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### B5. Per-call temporaries

| Line | Function | Before | After |
|---|---|---|---|
| 864 | `seed_with_top_level` | `let mut prelude = Vec::new();`, read by a `run_all` closure | `Vec::new_in(self.frame)`; the closure captures `let prelude: &[_] = &prelude;` |
| 2094 | `resolve_macros` | `mem::replace(&mut self.unresolved_macros, mem::take(&mut self.macros_spare))` | `mem::replace(&mut self.unresolved_macros, mem::replace(&mut self.macros_spare, Vec::new_in(self.frame)))` |
| 2095 | `resolve_macros` | `mem::take(&mut self.round_resolved)` | `mem::replace(&mut self.round_resolved, Vec::new_in(self.frame))`; the `push_resolved` closure's parameter is `&mut FrameVec<'f, ResolvedCall>` |
| 2118 | `resolve_macros` | `let mut eager_callback_buffer = vec![];` | `Vec::new_in(self.frame)` |
| 2393 | `resolve_macros` (attr) | `prev_active_attrs.entry(ast_id).or_default()` | `.or_insert_with(|| Vec::new_in(frame))`; `AttrMacroAttrIds::from_many(&active_attrs[..])` unchanged |
| 2431 | `resolve_macros` (derive) | `SmallVec::new()` for `derive_call_ids` | unchanged: it goes into the owned `MacroCallKind`/derive record (result data) |
| 2662 | `expand_round` | `mem::take(&mut self.round_calls)` | `mem::replace(.., Vec::new_in(self.frame))`; `expand_all` already takes `&[MacroCallId]` |
| 3342 | `process_macro_use_extern_crate` | `let mut single_imports = Vec::new();` then `Option<Vec<Name>>` to `import_macros_from_extern_crate` | `Vec::new_in(frame)`; the parameter becomes `Option<&[Name]>` |
| 3391, 3420 | `collect_module` | `&mut self.def_collector.path_buf` | `blaze_mem::scratch::with_string(|buf| ...)` around each `descend_into_definition` / `resolve_declaration` call (`mod_resolution.rs` keeps `buf: &mut String`); `path_buf` leaves the struct |
| 3780 | `collect_macro_call` | `let mut eager_callback_buffer = vec![];` per macro call | `Vec::new_in(self.def_collector.frame)` |
| `MacroDirectiveKind::Attr` | `attr: Attr` cloned from the item tree (an `Attr` owns `Option<Box<AttrInput>>`, so 1 to 3 allocations per attribute directive) | `attr: &'db Attr` (`Attrs<'db>::iter` yields `&'db Attr`); `resolve_attributes(&mut self, attrs: Attrs<'db>, ..)` |

`expand_all` (line 144) captures `calls: &[MacroCallId]` already.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

## Part C. `nameres/facts/`

The facts files are being written now (`driver.rs` and `macros.rs` changed while this was
read), so these are rules for each function. The line numbers are as read. The query graph has
no cycle (§G). Values carried between queries (`SccState.state: Arc<UnitState>`, `MacroStep`,
`ModuleDecls`, `SideTables`) are memo values: they stay owned. Everything a query makes
and drops in its run goes to the frame.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### C1. Frame plumbing

`CollectCx<'db>` (`env.rs`) becomes `CollectCx<'db, 'f>` with `frame: &'f Frame`.
`collect::cx(db, krate, in_block)` becomes `cx(db, frame, krate, in_block)`, called inside each
query's `current`. `CollectCx.deps: FxIndexMap<Name, BuiltDependency>` becomes
`FrameVec<'f, (Name, &'db BuiltDependency)>` (the rule of B1). Each query that makes a `cx` opens
`blaze_mem::frame::current(|frame| ..)` at the top of its body. That is `module_decls`,
`macro_step`, `round_env`, `scc_state`, `import_shape`, `partition`, `round_modules`,
`module_round` and `legacy_exports`, plus `collect_crate`/`collect_block`, which get the frame from
A1/A2.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### C2. Per query

| Query / function (file:line) | Temporary before | After | Owned result: how it is made |
|---|---|---|---|
| `round_modules` (driver.rs:56) | `order`, `rows` `Vec::new()` push, `index` `FxHashMap`, `stack` `vec![root]` | `order`/`rows` in `FrameVec`, `stack` `FrameVec`; `index` stays owned (it is in `RoundModules`) but is made `with_capacity_and_hasher(order.len())` after the walk, from the frame `order` | `order: Box::from(&order[..])`, `rows` moved by `collect::<Box<[_]>>()` from `order.drain(..)`'s twin (`TrustedLen`): one allocation each, no realloc |
| `round_modules` later rounds (driver.rs:90) | `previous.index.clone()` then inserts | build the new index once at `rows.len()` | as above |
| `module_round` (driver.rs:109, 151) | `ops` pushed, `ops.shrink_to_fit()` | frame vector of `DeclOp` | `OpLog` from it (C3) |
| `import_shape` (driver.rs:189, 200, 207) | a `collect()` (line 200) of unknown length, `out` `with_capacity(order.len())` | the line-200 list in a `FrameVec` | `modules: out.into_boxed_slice()` is exact (filled to its capacity) |
| `ShapeScratch` (driver.rs:267) | its vectors | `FrameVec`s, built with `cx.frame` | none |
| `collect_crate` (driver.rs:434) | `crate_ops.diagnostics: Vec::new()` push, `unresolved_extern_crates: FxHashSet`, `states: Vec<&SccState>`, `derive_helpers_in_scope` `FxHashMap<_, Vec>` with `or_insert_with(Vec::new)`, `pending: Vec<&MacroStep>`, `FinishSink`'s pushes into `def_map.diagnostics` | `FrameVec`s, `FrameSet`, the helpers as `FrameMap<_, FrameVec<DeriveHelper>>`; `FinishSink` gains `diagnostics: &mut FrameVec<'f, DefDiagnostic>` | `def_map.publish(diagnostics, helpers)` (A3) then `def_map.settle()` in place of `shrink_to_fit` at 557; `local` is exact already (`CrateEnv::local_def_map` reserves exactly); `Arc::make_mut(&mut crate_ops.data).shrink_to_fit()` (479) becomes `.settle()` (A5) |
| `collect_block` (driver.rs:693) | `unresolved_extern_crates`, `pending: Vec<MacroOp>`, `unresolved: Vec<DefDiagnostic>` `collect()` (753), `side: SideTables::default()` | `FrameVec`s, `FrameSet`; `side` is dropped in the run, so the block path's `SideTables` maps become a frame `SideTablesIn<'f>` (C4) | `publish` (A3) with `diagnostics.extend(unresolved)` into the frame vector first; `settle` at 766 |
| `macros::resolve_round` (macros.rs:75): `kept`, `added`, `resolved`, `eager` | `Vec::with_capacity`/`Vec::new()` | `Vec::new_in(cx.frame)` / `with_capacity_in`; `pending: &mut Vec<MacroOp>` becomes `&mut FrameVec<'f, MacroOp>` (and `Sink::take_macros(&mut self, into: &mut FrameVec<'f, MacroOp>)`) | none |
| `macros::expand_round` (macros.rs:545) | `calls` `with_capacity` | `with_capacity_in(n, cx.frame)`, passed as `&calls[..]` | none |
| `macros::resolve_one` derive (macros.rs:333) | `SmallVec::new()` for `derive_call_ids` | unchanged (result data, B5) | |
| `macro_step` (macros.rs:658) | `Carried::pending` (757) `with_capacity`, `StepSink` vectors `macros`, `imports`, `new_modules` (859 to 862), the view `DefMap` (840 to 846), `scope.scope(db).clone()` and `children(db).clone()` (826, 828) | `FrameVec`s for `macros`/`imports`/`pending`; `new_modules` in a `FrameVec`; the view map's tables stay owned (they are a `DefMap` the resolver reads) | `pending: Box::from_iter(pending.drain(..))`, `new_modules: Box::from_iter(..drain(..))` (one allocation each, in place of `into_boxed_slice`), `ops` per C3 (874: `shrink_to_fit` goes) |
| `partition` / `graph.rs` | `ImportGraph::from_edges`'s `cursor`, `by_target`, `offsets`, `targets`; `components`' `rindex`, `frames`, `stack`; `Solver`'s `paths`, `level_path`, `level_first`, `pair_ids`, `pairs`, `module_pairs`, `module_globs`, `links`, `seen`, `work`, `prelude_levels`, `edges` | each a `FrameVec`/`FrameMap`/`FrameSet` presized as today with `with_capacity_in`; `ImportGraph<'f>`, `Solver<'f>` | `Partition { of, starts, members }`: `of` is `rindex` once it is final, so `Box::from(&rindex[..])`; `starts`/`members` `Box::from(&v[..])` |
| `legacy_exports` (queries.rs) | `.filter_map().collect()` | count, then `with_capacity` + `extend` + `into_boxed_slice` (A7's form) | exact |
| `subtree_macro_exports`, `unresolved_named`, `round_progress`, `round_kind`, `latest`, `fact_handle`, `crate_root`, `crate_data`, `root_decl`, `child_key`, `decl_handle`, `row` | no temporaries, or a key | `current` not needed where nothing is allocated; tag still `Nameres` | ids and `Copy` values |
| `module_impls` (impls.rs:30) | none | none | exact already (`with_capacity` filled to it): one allocation per non-empty box. Budget test (b) holds it there |

`graph.rs`'s local `struct Frame` (the DFS stack entry) is renamed `DfsFrame` so it does not clash
with `blaze_mem::Frame`. The graph tests (`facts/tests/graph.rs`) call `components`,
`import_graph` and `partition` outside a query, so each test body is wrapped in
`blaze_mem::frame::task(Tag::Nameres, |frame| ..)` and passes `frame`.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### C3. `OpLog` (`ops.rs`)

Before: `OpLog { ops: Vec<DeclOp> }`, grown by `push`, then `shrink_to_fit`. After:
`OpLog { ops: Box<[DeclOp]> }` with `OpLog::from_frame(ops: FrameVec<'_, DeclOp>) -> OpLog { OpLog { ops: ops.into_iter().collect() } }`.
That is one allocation, since `vec::IntoIter` is `TrustedLen`. Producers (`collect.rs`'s sink,
`StepSink`, `module_round`) push into a `FrameVec` and call it once. `Update.resolutions:
Box<[(Option<Name>, PerNs)]>` is made from the collector's `held` slice with `Box::from(&held[..])`.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### C4. What `UnitState` carries (`unit.rs`, not yet written; `nameres-core.md` §5)

`UnitState` is carried to the next round in `SccState.state: Arc<UnitState>`, so it is owned. Its
per-round queues are made again each round from what is carried, so they are split into a frame
struct the round makes:

```rust
/// One round's queues over a unit's imports: made from the carried `UnitState` when the round
/// starts, dropped when it ends.
struct ImportQueues<'f> {
    pending_unresolved: BinaryHeap<Reverse<u32>, &'f Frame>,
    deferred_unresolved: FrameVec<'f, u32>,
    pending_indeterminate: BinaryHeap<Reverse<u32>, &'f Frame>,
    deferred_indeterminate: FrameVec<'f, u32>,
    waits: FrameMap<'f, WaitOn, u32>,
    edges: FrameVec<'f, WaitEdge>,
    any_waiters: FrameVec<'f, (u32, u32)>,
    keys: FrameVec<'f, (WaitOn, u8)>,
    woken: FrameVec<'f, u32>,
}
```

The carried part keeps `directives`, `list`, `seq`, `indeterminate`, `named`/`next_named`, the
glob edges, `from_glob_import` and the rows. When a carried `Vec` is cloned into a new state, it is
cloned at its length (`Vec::clone` allocates `len`, not capacity), so the carried state is exact.
The block path, whose state is never carried, uses the frame forms of B2 and B3 throughout. The
same split applies to `SideTables` (`env.rs`): `SideTablesIn<'f>` with frame maps for the block
path, and the owned `SideTables` for what `MacroStep` carries.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### C5. `env.rs`

`CrateEnv::after` (`into_vec` + `push` + `into_boxed_slice`, which reallocates when the vector has
slack) becomes `let mut v = Vec::with_capacity(self.list.len() + 1); v.extend_from_slice(&self.list); v.push(new); v.into_boxed_slice()`.
`install` (`macro_use_prelude.clear()` + `reserve(n)`) is exact already.

> frontend-nameres-core (owner of `nameres/facts/` but `graph.rs`/`impls.rs`): C5 applied (`CrateEnv::after` builds the grown list at exact size, `env.rs` `set_or_push`). C1-C4 open, owner frontend-nameres-core: they need `blaze_mem::frame::current` (blaze-mem has `frame::task` only today) and hir-def's blaze-mem dependency (H1-H3); `facts/` adopts them in one pass once both land, with `collect_crate`/`collect_block` taking `frame` exactly as A1/A2 call them. Note: `ModuleDecls::side` and `MacroStep::side` are now `Arc<SideTables>`, and `SccState::unresolved` is `SccState::imports: Box<[(ImportOp, Option<PerNs>)]>`.
> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

## Part D. The other files of the slice

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### D1. `assoc.rs`

`TraitItems::query_with_diagnostics` (line 48) and `ImplItems::of` (115): the body goes inside
`blaze_mem::frame::current(|frame| ..)`, and `AssocItemCollector::new` takes `frame`.

| Field (line) | Before | After |
|---|---|---|
| `items` (147) | `Vec<(Name, AssocItemId)>`, `into_boxed_slice()` at 188 (realloc with slack) | `FrameVec<'f, (Name, AssocItemId)>`; result `Box::from_iter(self.items.drain(..))` (one allocation) |
| `macro_calls` (148) | `ThinVec`, `shrink_to_fit()` at 187 | `FrameVec<'f, (AstId<ast::Item>, MacroCallId)>`; result `ThinVec::with_capacity(n)` + `extend` (exact; empty allocates nothing) |
| `diagnostics` (143) | `Vec<DefDiagnostic>` | `FrameVec<'f, DefDiagnostic>`; `DefDiagnostics::new` takes it (D4) |

`collect` returns `(Box<[(Name, AssocItemId)]>, ThinVec<..>, DefDiagnostics)`. Line 59's
`DefDiagnostics::new(vec![])` becomes `DefDiagnostics::default()` (no allocation either way).

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### D2. `uses.rs`

`used_imports` (`db: &Db`): body in `current`. `Walk { uses, unresolved, glob_targets }` become
`FrameVec`s and a `FrameMap`. `store()`'s `resolvers` becomes `FrameMap`, and `Within::of`'s
`pats`, `types` and `todo` become frame collections, with the frame passed down from the query.
`finish`'s `imports`, `macro_use` and `globs` (`FxIndexMap<_, Vec<Name>>`), `ambiguous` and `seen`
become frame collections (`indexmap::IndexMap` has no allocator parameter, so they become a
`FrameMap<K, u32>` index into a `FrameVec<(K, FrameVec<'f, Name>)>`, which keeps insertion order).
Each owned result box is made with `Box::from(&v[..])` or a `TrustedLen` collect. `path_uses` and
`path_uses_outside_imports` take `out: &mut Vec<Use, A>` with `A: Allocator` (callers in
`collector.rs` pass frame vectors; IDE callers keep `Vec<Use>`).

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### D3. `demand.rs`

`macros_declaring_roots` and `macros_writing_diagnostic_items`: body in `current`. `Look`'s
`known` (`FxHashSet`), `found`, `calling`, `seen` and `settle`'s `writing` become frame collections.
The result `names` is collected from a hash set (exact length), sorted, then `.into()`. It is
built from a frame vector with `Box::from(&v[..])` after `sort_unstable`. `macro_text` and `words`
(`Vec<Symbol>`, per call) become `FrameVec`. The public non-query walkers (`scope_body_owners`,
`module_body_owners`, `crate_body_owners`, `body_blocks`, `walk_bodies`, `every_body`,
`bodies_declaring_roots`, `walk_bodies_saying`) run from drivers outside a query. They keep their
signatures, and `walk_bodies`' `seen`/`queue` and `walk_bodies_saying`'s `local` move to
`thread_local!` `blaze_mem::scratch::Scratch<T>` buffers (`take()`, cleared on entry). A nested call
gets its own buffer through `scratch::lend_u32s` (depth 4), and depth 5 is blaze-mem's panic
naming the caller, not a heap fallback.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### D4. `diagnostics.rs`

```rust
impl DefDiagnostics {
    pub fn new<A: Allocator>(diagnostics: Vec<DefDiagnostic, A>) -> Self {
        Self(
            diagnostics
                .is_empty()
                .not()
                .then(|| triomphe::ThinArc::from_header_and_iter((), diagnostics.into_iter())),
        )
    }
}
```

`vec::IntoIter<T, A>` is `ExactSizeIterator`, which is what `from_header_and_iter` needs: one
allocation at exact size, as today.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### D5. `frozen.rs`

`served(db, krate)` becomes `served_in(db, krate, frame)`, called from A1 inside the frame.
`Dec<'a>` becomes `Dec<'a, A: Allocator + Copy = Global>`, with a private
`Dec::new_in(db, krate, bytes, alloc: A)`. The public `Dec::new` (used by
`hir-ty/src/method_resolution/frozen.rs`, `deps/signatures/table.rs`, `demand_tests.rs`) is
`new_in(.., Global)`, unchanged for them. The decode temporaries become `Vec<_, A>` /
`hashbrown::HashMap<_, _, FxBuildHasher, A>`: `closure` (and `closure()`'s set), `paths`, `files`,
`calls`, `contexts`, `own`, `modules: HashMap<u32, Vec<ModuleId, A>, _, A>`, and `def_map`'s
`skeleton`. `many()` gets `many_in(alloc)`. The owned maps it fills (`macro_use_prelude`,
`derive_helpers_in_scope`, `exported`, `functions`, `extern_prelude`) are built
`with_capacity(n)` from the frame vector (`n` is known from the encoding) in place of
`into_iter().collect()`. `derive_helpers_in_scope` values are `Box::from(&v[..])` (A3).
`Dec::def_map`'s `super::ModulesMap::new()` becomes `ModulesMap::with_capacity(n)`, and
`Dec::scope` is exact already (`reserve_exact`). `check` (`Mode::Check`, testing) uses the same
frame.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

### D6. `mod_resolution.rs`, `proc_macro.rs`, `attr_resolution.rs`

- `ModDir::resolve_declaration(.., buf: &mut String)` stays. The caller's buffer comes from
  `scratch::with_string` (B5). The `Box<[String]>` candidates of an unresolved-module diagnostic
  are diagnostic data (owned).
- `parse_macro_name_and_helper_attrs` (`proc_macro.rs:85`): the `filter_map(..).collect::<Box<[_]>>()`
  is a filter (it grows, then shrinks). After:

```rust
        let ident = |tt: &TtElement<'_>| match tt {
            TtElement::Leaf(Leaf::Ident(helper)) => Some(helper.as_name()),
            _ => None,
        };
        let mut out = Vec::with_capacity(helpers.clone().filter(|tt| ident(tt).is_some()).count());
        out.extend(helpers.filter_map(|tt| ident(&tt)));
        let helpers = out.into_boxed_slice();
```

  (`TtIter` is `Clone`.)
- `attr_macro_as_call_id` clones the attribute's `tt` into the call's owned argument (result data
  of the interned call). No change.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

## Part E. Requests to the other slices

E1, to **mem-db-def-resolve** (`item_scope.rs`):

- `PerNsGlobImports` becomes `PerNsGlobImports<A: Allocator + Clone = Global>` with its three sets
  `hashbrown::HashSet<(ModuleId, Name), FxBuildHasher, A>` and `PerNsGlobImports::new_in(alloc)`.
  `push_res_with_import(&mut self, glob_imports: &mut PerNsGlobImports<A>, ..)` is generic over
  `A`.
- `ItemScope::shrink_to_fit` becomes `ItemScope::settle`: each column made again at its length
  (`Vec::with_capacity(len)` + `extend(drain)`, maps by `exact_map`), not shrunk.
- Nothing on the hit path changes (`get`, `resolve_path_fp*` keep their signatures).

E2, to **mem-db-def-items**: none from this slice. `AstIdWithPath` and `ModPath` are interned or
`Copy` already, so directive clones allocate nothing.

E3, to **mem-db-def-expand** (`hir-expand/src/proc_macro.rs`): `ProcMacros::list(ctx)` returns a
new `Box<[(Name, CustomProcMacroExpander, bool)]>` on every collection of a proc-macro crate. It
should be `list_in(ctx, frame: &Frame) -> Option<&[(Name, CustomProcMacroExpander, bool)]>`
(frame-allocated), or an iterator. The collector's `proc_macros` field then becomes
`&'f [..]`.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

## Part F. Budget tests (new file `nameres/tests/budget.rs`, `mod budget;` in `nameres/tests.rs`)

They need H3's `#[global_allocator] static ALLOC: blaze_alloc_count::Counting` (mem-db-def). Every
batch runs on the calling thread (`TestDB`'s `RUN_ORDER`), so per-thread counts see the whole
collection.

```rust
//! Allocation budgets of def-map collection (docs/v2/plan/lanes/mem/db.md, point 7): a
//! re-collection that changes nothing allocates nothing, and a miss allocates what its owned
//! result holds and no more.

use std::hint::black_box;

use blaze_alloc_count::{assert_at_most, measure};
use test_fixture::WithFixture;

use crate::{
    ModuleId, file_item_tree,
    nameres::{crate_def_map, facts::impls::module_impls},
    test_db::TestDB,
};

/// A crate whose last file holds only a function body: whitespace there changes no item tree.
const CRATE: &str = r#"
//- /lib.rs crate:main
mod a;
mod b;
pub use a::*;
//- /a.rs
macro_rules! make { ($name:ident) => { pub struct $name; } }
make!(FromMacro);
pub trait Tr {}
impl Tr for FromMacro {}
pub mod inner { pub use super::FromMacro as Again; }
//- /b.rs
pub fn body() {}
"#;

/// (a): collect cold, edit whitespace in `b.rs`'s body twice. The first re-collection warms what
/// a revalidation keeps (salsa's verification scratch, blaze-mem's scratch); the second one is
/// identical and allocates nothing. The item tree of the edited file is made again outside the
/// measure: it is `Tag::Parse`'s miss, not collection's, and it backdates.
#[test]
fn a_recollection_after_an_edit_that_changes_no_item_tree_allocates_nothing() {
    let (mut db, files) = TestDB::with_many_files(CRATE);
    let krate = db.fetch_test_crate();
    let b = files[2];
    black_box(crate_def_map(&db, krate));

    let edit = |db: &mut TestDB, text: &str| {
        let file = b.file_id(db);
        db.set_file_text(file, text);
        black_box(file_item_tree(db, b.into(), krate));
    };
    let recollect = |db: &TestDB| {
        measure(|| {
            let def_map = crate_def_map(db, krate);
            black_box(def_map.root);
        })
        .1
    };

    edit(&mut db, "pub fn body() { }\n");
    recollect(&db);
    edit(&mut db, "pub fn body() {  }\n");
    let executed = db.log_executed(|| {
        black_box(crate_def_map(&db, krate));
    });
    assert!(
        !executed.iter().any(|it| it.contains("crate_local_def_map")),
        "a whitespace edit in a body re-collected the crate: {executed:#?}"
    );

    edit(&mut db, "pub fn body() {   }\n");
    let used = recollect(&db);
    assert_eq!(
        used.allocations, 0,
        "re-collecting after an edit that changes no item tree allocated {} times ({} bytes)",
        used.allocations, used.bytes
    );
}

/// (a), the hit path: once collected, reading a module's scope through the def map allocates
/// nothing.
#[test]
fn reading_a_collected_scope_allocates_nothing() {
    let (db, files) = TestDB::with_many_files(CRATE);
    let krate = db.fetch_test_crate();
    let a = db.module_for_file(files[1].file_id(&db));
    let def_map = crate_def_map(&db, krate);
    let name = hir_expand::name::Name::new_symbol_root(intern::Symbol::intern("FromMacro"));
    let warm = def_map.module(&db, a).name_in_scope(&db, &name);
    assert!(warm.take_types().is_some(), "the fixture's `FromMacro` is not in `a`");
    black_box(def_map.module(&db, a).traits_in_scope(&db).count());
    assert_at_most(0, "a hit of a collected module's name", || {
        let def_map = crate_def_map(&db, krate);
        black_box(def_map.module(&db, a).name_in_scope(&db, &name));
        black_box(def_map.module(&db, a).traits_in_scope(&db).count());
    });
}

/// The boxes a `ModuleImpls` owns: an empty box allocates nothing.
fn owned_boxes(lens: &[usize]) -> u64 {
    lens.iter().filter(|&&len| len > 0).count() as u64
}

/// What one first run of `f` allocates, with everything it reads already computed.
fn miss(f: impl FnOnce()) -> u64 {
    measure(f).1.allocations
}

/// (b): a miss of `module_impls` allocates what its result holds and no more. Salsa's own cost of
/// a run (its memo, its dependency list) is read from the same query on a module whose result
/// owns nothing: both runs read the same inputs, so they record the same dependencies. A first
/// run on a third module (the root) takes the query's one-time setup outside both measures.
#[test]
fn a_miss_of_module_impls_allocates_its_result_alone() {
    let (db, files) = TestDB::with_many_files(
        r#"
//- /lib.rs crate:main
mod plain;
mod with_impls;
pub struct Root;
//- /plain.rs
pub struct P;
//- /with_impls.rs
pub struct S;
pub trait Tr {}
impl S { fn f() {} }
impl Tr for S {}
impl Tr for () {}
"#,
    );
    let krate = db.fetch_test_crate();
    let plain = db.module_for_file(files[1].file_id(&db));
    let with_impls = db.module_for_file(files[2].file_id(&db));
    let warm = |module: ModuleId| {
        let def_map = crate_def_map(&db, krate);
        black_box(def_map.module(&db, module));
    };
    let root = db.module_for_file(files[0].file_id(&db));
    warm(root);
    warm(plain);
    warm(with_impls);
    black_box(module_impls(&db, root));

    let salsa = miss(|| {
        black_box(module_impls(&db, plain));
    });
    let empty = module_impls(&db, plain);
    assert_eq!(
        owned_boxes(&[empty.trait_impls.len(), empty.inherent_impls.len(), empty.builtin_derive_impls.len()]),
        0,
        "the baseline module has impls"
    );

    let used = miss(|| {
        black_box(module_impls(&db, with_impls));
    });
    let impls = module_impls(&db, with_impls);
    let owned = owned_boxes(&[
        impls.trait_impls.len(),
        impls.inherent_impls.len(),
        impls.builtin_derive_impls.len(),
    ]);
    assert_eq!((impls.trait_impls.len(), impls.inherent_impls.len()), (2, 1), "fixture changed");
    assert_eq!(
        used,
        salsa + owned,
        "a miss of `module_impls` allocated {used}: salsa's run is {salsa}, its result owns {owned} boxes"
    );
}

/// (b): a miss of `ModuleScope::traits` allocates its box (none when the module has no trait),
/// on top of the same run's cost on a module without traits.
#[test]
fn a_miss_of_a_modules_traits_allocates_its_box_alone() {
    let (db, files) = TestDB::with_many_files(
        r#"
//- /lib.rs crate:main
mod none;
mod some;
//- /none.rs
pub struct N;
//- /some.rs
pub trait A {}
pub trait B {}
pub struct S;
"#,
    );
    let krate = db.fetch_test_crate();
    let none = db.module_for_file(files[1].file_id(&db));
    let some = db.module_for_file(files[2].file_id(&db));
    let def_map = crate_def_map(&db, krate);
    let (none_data, some_data) = (def_map.module(&db, none), def_map.module(&db, some));
    // The query's one-time setup, outside both measures: the root has no trait either.
    black_box(def_map.module(&db, def_map.root).traits_in_scope(&db).count());

    let salsa = miss(|| {
        black_box(none_data.traits_in_scope(&db).count());
    });
    assert_eq!(none_data.traits_in_scope(&db).count(), 0, "the baseline module has traits");

    let used = miss(|| {
        black_box(some_data.traits_in_scope(&db).count());
    });
    let traits = some_data.traits_in_scope(&db).count();
    assert_eq!(traits, 2, "fixture changed");
    let owned = u64::from(traits > 0);
    assert_eq!(used, salsa + owned, "salsa's run is {salsa}, the result owns {owned} box");
}
```

When mem-db-salsa makes a run's memo and dependency list allocation-free (its pooled memos), the
baseline `salsa` measured here goes to 0, and both (b) tests then hold a miss to exactly
`owned`. They need no edit for that. A crate-wide miss (`crate_local_def_map` cold) is not held to
an exact count until the A4 tables leave the global heap (notes, "Residual"). It is held by (a)
from the warm side.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).

## Part G. Salsa cycles

No nameres query has `cycle_fn`, `cycle_initial` or `cycle_result` (`rg` over `nameres.rs` and
`nameres/`). The one in hir-def is `expr_store/lower.rs:4236`, outside this slice. The collector's
fixpoints (`resolution_loop`, `FIXED_POINT_LIMIT`) are loops inside one run, so one task. The facts
rounds are separate salsa keys (`RoundKey(krate, r)`). A round reads only round `r - 1`, so the
graph is acyclic by construction (`nameres-core.md` §3). What a round keeps for the next is the
owned memo `SccState { members, handles, prelude, upstream, unresolved: Box<[ImportOp]>, state: Arc<UnitState> }`
and `MacroStep { ops, pending, side, new_modules, .. }`. Both are owned and made at exact size per
C2 to C4, and nothing in them points into a frame.

> forwarded to frontend-nameres (`nameres/`) (from requests, pass 16; told by tmux).
