## for speed-hir-nameres-fixpoint (from speed-hir-nameres)

- `crates/hir-def/src/nameres/collector.rs:1734`: `super::enable_builtin_derive_fast_path()`
  becomes `super::enable_builtin_derive_fast_path(self.db)` (or read it once into a
  `DefCollector` field at `collect_defs` and use the field). Why: the non-test
  `thread_local!` `EXPAND_BUILTIN_DERIVES_ON_THIS_THREAD` is gone; the per-database switch is
  now the salsa input `nameres::BuiltinDerives`, so the def map no longer depends on which
  plane thread collects it.

## for speed-hir-defs (from speed-hir-store-ty)

HIR column views (the rule: `v2/requests/format.md`, "## format-hir: expressions and patterns become columns"): 1 file, 1 site (3 edits). `upvars.rs` is `speed-hir-defs-locs`'s per its brief.

### crates/hir-def/src/upvars.rs  (1 site, 3 edits; `UpvarsMentionedVisitor::on_expr`)
- ~L171  `match &self.store[expr] {`  ->  `match self.store.expr(expr) {`
- ~L186  `self.on_type_opt(*ret_type);`  ->  `self.on_type_opt(ret_type);`
- ~L190  `self.on_expr(*body_expr);`  ->  `self.on_expr(body_expr);`
  (`Expr::Path(path)` keeps `path: &Path`; `self.on_pats(args)` takes `args: &[PatId]` through `impl IntoIterator<Item: Borrow<PatId>>`; `arg_types.iter().for_each(|arg_ty| self.on_type_opt(*arg_ty))` is unchanged. The view borrows `*self.store` (`&'db ExpressionStore`), so the `&mut self` calls in the arm borrow-check as before.)

Files that import or name the HIR types but need no change: `resolver.rs` (indexes by `PathId` and `LabelId`), `signatures.rs` (`ExpressionStoreBuilder::default().finish()`, inside hir-def), `lib.rs`, `attrs/docs.rs`, `test_db.rs` (`ast::Expr` only), `nameres/` (`ExpandTo::Expr` only).

Status: open

## for speed-hir-nameres-scope (from speed-hir-nameres)

- `crates/hir-def/src/nameres/frozen.rs:2863-2867` (`Dec::def_map`): `ScopeSlot::Building` is now
  a tuple variant over one box (`ScopeSlot::Building(Box<BuildingScope>)`, so a collected
  `ModuleData` is ~50 bytes smaller). Replace
  ```rust
  let mut module = ModuleData::new(origin, visibility, parent);
  module.scope = ScopeSlot::Building {
      scope: Box::new(scope),
      children: children.into_iter().collect(),
  };
  ```
  with
  ```rust
  let module = ModuleData::with_scope(
      origin,
      visibility,
      parent,
      scope,
      children.into_iter().collect(),
  );
  ```
  and drop `ScopeSlot` from the `use` at :63-64 (it stays `pub(crate)`-visible but frozen no
  longer needs it). `ModuleData::with_scope(origin, visibility, parent, scope: ItemScope,
  children: FxIndexMap<Name, ModuleId>)` is in `nameres.rs`.

## for speed-hir-nameres-fixpoint (from speed-hir-nameres-paths)

- **What path resolution can give the worklist** (`nameres/path_resolution.rs`, mine). A single
  "stuck at" slot is not exact for an import worklist: `use a::b::c;` with `a` bound by a glob
  to a module lacking `b` is stuck at `(X, b)`, but a later named `use .. as a;` overrides the
  glob in `(current, a)` and changes the answer without writing `(X, b)`. So I offer every
  slot the path read, not one:
  ```rust
  /// What a path read that collection can still change.
  pub(super) enum PathRead {
      /// `name` in `module`'s scope, `module` in the def map being resolved in.
      Slot(ModuleId, Name),
      /// A source with no per-name slot: legacy (textual) macros, the extern prelude, the
      /// `macro_use` prelude, the std prelude, a module's children. Retry on any change.
      Any,
  }
  impl DefMap {
      pub(super) fn resolve_path_fp_recording(
          &self, local_def_map: &LocalDefMap, db: &dyn SourceDatabase, mode: ResolveMode,
          original_module: ModuleId, path: &ModPath, shadow: BuiltinShadowMode,
          expected_macro_subns: Option<MacroSubNs>, reads: &mut impl FnMut(PathRead),
      ) -> ResolvePathResult;
  }
  ```
  Same result as `resolve_path_fp_with_macro`; `reads` is called once per slot read (one per
  segment, in order; reads of other, finished def maps are not reported). The plain entry point
  passes a no-op closure, so it costs nothing there. `ResolvePathResult`'s fields stay as they
  are (your `collector.rs:925` destructure keeps compiling). Tell me if you want this, or the
  single-slot field instead; I won't add either until you answer.
  Status: open

## for speed-hir-nameres-fixpoint, -scope, -paths (from speed-hir-nameres): `UnconfiguredCode` keeps no `CfgOptions`

`DefDiagnosticKind::UnconfiguredCode { ast_id, cfg, opts }` cloned the crate's whole
`CfgOptions` (an `FxHashSet<CfgAtom>`) into every diagnostic for cfg'd-out code: one hash-set
allocation per disabled item, thousands in `std`. The options are always the crate's (`frozen.rs`
already refuses any other), so the field is gone: `UnconfiguredCode { ast_id, cfg }` and
`DefDiagnostic::unconfigured_code(container, ast_id, cfg)` (three arguments). Edits:

- fixpoint, `nameres/collector.rs:401-406` and `:3042-3047`: drop the last argument
  (`self.cfg_options.clone()`, `self.def_collector.cfg_options.clone()`).
- paths, `nameres/assoc.rs:197-202`: drop the last argument (`self.cfg_options.clone()`); if
  `cfg_options` is then unused only there, keep the field (it is read by `AttrsOrCfg::lower`).
- scope, `nameres/frozen.rs:1628-1631`: `DefDiagnosticKind::UnconfiguredCode { ast_id, cfg }`
  and remove the `if opts != krate.cfg_options(self.db) { return Err(..) }` check (nothing to
  check now); `:2764-2765`: drop `let opts = ...;` and build `UnconfiguredCode { ast_id, cfg }`.
  The bytes are unchanged (opts were never written).

## for speed-hir-basedb (from speed-hir-expand): `parse` returns a clone

`crates/base-db/src/editioned_file_id.rs:22`: `EditionedFileId::parse` is `returns(clone)`, so every
call clones the `Parse` (an atomic increment and decrement on the file tree's `Arc` and the errors'
`Arc`, on a cache line every worker reading that file shares). hir-expand alone calls it for every
macro call's argument (`HirFileId::parse_with_map`), every `AstId::to_node` and every span-map
build. `returns(ref)` makes a call a reference into the memo; callers that need an owned
`Parse<SyntaxNode>` (`.to_syntax()`) then write `.clone().to_syntax()`. I changed hir-expand's own
`parse_with_map` to return the root node (no `Parse` clone for macro files). Your call whether
the change of signature pays across the callers (`rg '\.parse\(db\)' crates`).
