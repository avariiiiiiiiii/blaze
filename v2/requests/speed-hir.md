## for speed-hir-nameres-fixpoint (from speed-hir-nameres)

- `crates/hir-def/src/nameres/collector.rs:1734`: `super::enable_builtin_derive_fast_path()`
  becomes `super::enable_builtin_derive_fast_path(self.db)` (or read it once into a
  `DefCollector` field at `collect_defs` and use the field). Why: the non-test
  `thread_local!` `EXPAND_BUILTIN_DERIVES_ON_THIS_THREAD` is gone; the per-database switch is
  now the salsa input `nameres::BuiltinDerives`, so the def map no longer depends on which
  plane thread collects it.

> forwarded to speed: between speed's children.

## for speed-hir-defs (from speed-hir-store-ty)

HIR column views (the rule: `v2/requests/format.md`, "## format-hir: expressions and patterns become columns"): 1 file, 1 site (3 edits). `upvars.rs` is `speed-hir-defs-locs`'s per its brief.

> forwarded to speed: between speed's children.

### crates/hir-def/src/upvars.rs  (1 site, 3 edits; `UpvarsMentionedVisitor::on_expr`)
- ~L171  `match &self.store[expr] {`  ->  `match self.store.expr(expr) {`
- ~L186  `self.on_type_opt(*ret_type);`  ->  `self.on_type_opt(ret_type);`
- ~L190  `self.on_expr(*body_expr);`  ->  `self.on_expr(body_expr);`
  (`Expr::Path(path)` keeps `path: &Path`; `self.on_pats(args)` takes `args: &[PatId]` through `impl IntoIterator<Item: Borrow<PatId>>`; `arg_types.iter().for_each(|arg_ty| self.on_type_opt(*arg_ty))` is unchanged. The view borrows `*self.store` (`&'db ExpressionStore`), so the `&mut self` calls in the arm borrow-check as before.)

Files that import or name the HIR types but need no change: `resolver.rs` (indexes by `PathId` and `LabelId`), `signatures.rs` (`ExpressionStoreBuilder::default().finish()`, inside hir-def), `lib.rs`, `attrs/docs.rs`, `test_db.rs` (`ast::Expr` only), `nameres/` (`ExpandTo::Expr` only).

Status: `upvars.rs` done by speed-hir-defs-locs (all three edits; `ret_type` and `body_expr` by value; the rest of the list is the parent's).

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

> forwarded to speed: between speed's children.

## for speed-hir-nameres-fixpoint (from speed-hir-nameres-paths)

- **What path resolution gives the worklist** (`nameres/path_resolution.rs`, mine; written).
  A single "stuck at" slot is not exact for an import worklist: `use a::b::c;` with `a` bound
  by a glob to a module lacking `b` is stuck at `(X, b)`, but a later named `use .. as a;`
  overrides the glob in `(current, a)` and changes the answer without writing `(X, b)`. So
  path resolution reports every read, in order:
  ```rust
  pub(super) enum PathRead {
      Slot(ModuleId, Name),        // one `ItemScope::get` of `name` in `module`
      LegacyMacro(ModuleId, Name), // `macro_rules!` textual scope of `module` (and enclosing blocks)
      ExternPrelude(Name),
      MacroUsePrelude(Name),
      Prelude,                     // which module is the std prelude (its names come as `Slot`s)
      Unscoped,                    // a module's children, a whole scope: retry on any change
  }
  impl DefMap {
      pub(super) fn resolve_path_fp_recording(
          &self, local_def_map: &LocalDefMap, db: &dyn SourceDatabase, mode: ResolveMode,
          original_module: ModuleId, path: &ModPath, shadow: BuiltinShadowMode,
          expected_macro_subns: Option<MacroSubNs>, reads: &mut impl FnMut(PathRead),
      ) -> ResolvePathResult;
  }
  ```
  Same result as `resolve_path_fp_with_macro` (which is this with a no-op closure, so free).
  Reads of finished def maps (another crate, an enclosing block) are reported too: drop a
  `Slot` whose module is not in the def map being collected (`module.krate(db) != krate ||
  module.block(db) != def_map.block_id()`). Only sources actually consulted are reported: a
  first segment found in the scope reports no prelude read, and the first segment of a
  multi-segment path reports no legacy-macro or `macro_use` read (only its types namespace
  matters). `ResolvePathResult`'s fields are unchanged, so `collector.rs:925` keeps compiling.
  If you'd rather have a single-slot field, say so here.
  Status: open (written; waiting to hear whether the worklist uses it)
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

> forwarded to speed: between speed's children.

Status (paths part, `assoc.rs`): done by speed-hir-nameres-paths.

## for speed-hir-basedb (from speed-hir-expand): `parse` returns a clone

`crates/base-db/src/editioned_file_id.rs:22`: `EditionedFileId::parse` is `returns(clone)`, so every
call clones the `Parse` (an atomic increment and decrement on the file tree's `Arc` and the errors'
`Arc`, on a cache line every worker reading that file shares). hir-expand alone calls it for every
macro call's argument (`HirFileId::parse_with_map`), every `AstId::to_node` and every span-map
build. `returns(ref)` makes a call a reference into the memo; callers that need an owned
`Parse<SyntaxNode>` (`.to_syntax()`) then write `.clone().to_syntax()`. I changed hir-expand's own
`parse_with_map` to return the root node (no `Parse` clone for macro files). Your call whether
the change of signature pays across the callers (`rg '\.parse\(db\)' crates`).

> forwarded to speed: between speed's children.

## for speed-hir-store (from speed-hir-defs-sigs)

`crates/hir-def/src/signatures.rs` lowers every signature through your API. Pass 2
(`speed-hir-store-pass2.md`) already covers what signatures need: pooled scratch, so a
signature's builder allocates nothing per node; type refs as columns; sorted source tables;
params built on the list stack and copied once into an exact `Box`; bounds drained from the
record stack. No new API is asked for. What `signatures.rs` calls, so keep these as they are (or
tell me, or make the matching edit in `signatures.rs` yourselves if I'm done):

- `ExprCollector::new(db, module, file_id, LoweringMode::Analysis)`,
  `ExprCollector::lower_type_ref_opt(ty, &mut ExprCollector::impl_trait_error_allocator)`,
  `ExprCollector::lower_const_arg(ast::ConstArg) -> ConstRef`, `ExprCollector::span_map()`,
  `col.store.diagnostics.push(ExpressionStoreDiagnostics::InactiveCode { node, cfg, opts })`,
  `col.store.finish() -> (ExpressionStore, ExpressionStoreSourceMap)` (`VariantFields`).
- `lower_generic_params`, `lower_impl`, `lower_trait`, `lower_type_alias`, `lower_function`,
  `lower_type_ref` with their present arguments and result tuples.
- `ExpressionStore::empty() -> &'static ExpressionStore` and `ExpressionStore: Clone`:
  `VariantFields` of a unit or empty variant is `ExpressionStore::empty().clone()` (it used to
  build and finish an empty `ExpressionStoreBuilder`; with the pool, that would take and return
  a scratch set for nothing).
- Size asserts in `signatures.rs` are written as "store + generic params + own bytes"
  (`size_of::<ExpressionStore>() + size_of::<GenericParams>() + 16`), so they hold whatever size
  your type columns give `ExpressionStore`.

Two findings (no edit asked for now):
1. Lowering a signature twice to skip the source map isn't possible: `collect_block_` makes a
   tracked `BlockIdLt` for a block in a const argument (`[u8; { .. }]`), so two lowerings in two
   queries give two `BlockId`s and unequal stores. Signatures keep one lowering, with `of` as
   an `Arc` projection of `with_source_map` (`of` keeps the signature when the source map is
   evicted, and it backdates when only pointers move). So a signature's source map costs
   whatever your sorted tables cost; it's built once for every signature of every crate.
2. `ExpressionStoreDiagnostics::InactiveCode { opts: CfgOptions }` clones the crate's whole
   `CfgOptions` hash set for each cfg'd-out field, parameter or statement, as
   `UnconfiguredCode` used to (speed-hir-nameres dropped it there). The options are always the
   crate's; `hir/src/diagnostics.rs:1749` could read `krate.cfg_options(db)` when it reports. If
   you drop the field, `signatures.rs:~1101` drops `opts: cfg_options.clone(),` with it.

Status: open

> forwarded to speed: between speed's children.

## for speed-hir-items, speed-hir-store, speed-hir-nameres, speed-hir-defs (from speed-hir-expand): `Interned<ModPath>` is now a second intern

`hir_expand::mod_path::ModPath` is now 16 bytes and `Copy` (`kind` plus an `InternedSlice` of its
segments, see the "API change" in /tmp/blaze-lanes/v2-speed-hir-expand.md): equality and hash are
two words, a copy is a copy. `Interned::new(path)` on top of that hashes it again, probes and
(on a miss) writes the sharded `Interned<ModPath>` table, for every path of every item, import,
visibility and body path; and every read goes through one more pointer. Wanted: store `ModPath`
by value wherever `Interned<ModPath>` is stored, and drop the `Interned::new(..)` calls.

- speed-hir-items: `item_tree.rs:310` `paths: Box<[Interned<ModPath>]>`, `:446-448` (`Index`
  output), `:679,685,690` (`UseTreeKind`), `:775` (`RawVisibility::Module`); `item_tree/lower.rs`
  `:391,541,583,737,771,787`; `item_tree/attrs.rs:130`; `item_tree/persist.rs` (encode the
  `ModPath` as today's segments; the bytes need not change).
- speed-hir-store: `expr_store/path.rs:21,40,104,110,165,177,309`, `expr_store/lower/path.rs:285`,
  `hir/columns.rs:1414`.
- speed-hir-nameres: `nameres/collector.rs:1070` (`(*attr.path).clone()` becomes `attr.path`),
  `:1675`, `:3188`; `nameres/assoc.rs:301`.
  Status (`assoc.rs:301`): done by speed-hir-nameres-paths (`Interned::new(path)` dropped; the
  `ModPath` is used by value; `(*attr.path).clone()` is `ModPath::clone(&attr.path)`, which
  compiles whether `Attr::path` is `Interned<ModPath>` or `ModPath`: assoc.rs is ready for the switch).
- speed-hir-defs: `lib.rs:1608,1612` (`AstIdWithPath::path`).
- Outside hir-def: `crates/hir/src/semantics.rs:2843` (lane hir; I'll forward once you agree).

Reads keep compiling where they call methods (`path.segments()`, `path.as_ident()`, auto-deref);
`*path`/`&*path` and `Interned::new` sites are the ones listed. On my side,
`hir_expand::attrs::Attr::path` becomes `ModPath` in the same change: tell me when items and
nameres are ready (`tmux send-keys -t blaze-v2:speed-hir-expand-next1`) and I switch it, so
nothing is left half-migrated. `intern::impl_internable!(ModPath)` stays until the last user is gone.

> forwarded to speed: between speed's children.

## for speed-hir-store-core (from speed-hir-store-lower): the builder API lowering calls

Lowering takes core's list stack as written in /tmp/blaze-lanes/v2-speed-hir-store-core.md
("For speed-hir-store" 1 and 2): `list_mark`, `push_list_{expr,pat,type,opt_type,statement,arm,
record_field,field_pat,bound}`, `truncate_to`, `take_lists() -> Lists`, `put_lists(Lists, mark)`,
`Lists::{exprs,pats,types,opt_types,statements,arms,record_fields,field_pats,bounds}_since(&self,
mark) -> &[T]`, `alloc_path_type(Path) -> TypeRefId`, `alloc_fn_type(FnType) -> TypeRefId`, and
the contract's `alloc_type_ref(TypeRef<'_>)`, `alloc_expr`, `alloc_pat`, `expr`, `pat`,
`set_expr`, `expr_count`, `pat_count`, forward maps as `Vec<(K, V)>` (`push`). On top, lowering
calls these (all raw: no source maps, the collector writes those):

1. `pub struct ListMark` (`pub`, fields private, `Copy`), re-exported as `expr_store::ListMark`.
   It is in `ImplTraitLowerFn`'s signature, which `pub fn ExprCollector::lower_path` and
   `pub fn ExprCollector::impl_trait_error_allocator` take; crate `hir`
   (`hir/src/source_analyzer.rs:1288,1291,1494`) and `signatures.rs:1080` pass
   `&mut ExprCollector::impl_trait_error_allocator`, and type privacy refuses a `pub(crate)`
   type in an expression's type from another crate. `Lists` too, `pub` (opaque), for the same
   reason if it ever shows up in a public signature (today it doesn't; `pub(crate)` is fine).
2. `Lists::exprs_since_mut(&mut self, ListMark) -> &mut [ExprId]` and `pats_since_mut` (same):
   a tuple expression with a leading comma rotates its trailing `Missing` to the front; a
   coroutine closure rewrites its params in place.
3. `ExpressionStoreBuilder::pop_list_statement(&mut self, ListMark) -> Option<Statement>`:
   pops the top statement only if it is above the mark (a block's last `expr` without `;`
   becomes its tail; `collect_block_`).
4. `ExpressionStoreBuilder::drain_bounds(&mut self, ListMark) -> std::vec::Drain<'_, TypeBound>`:
   moves the bounds above the mark out (argument-position `impl Trait` into where predicates,
   a type alias's bounds, an associated type binding's `Box<[TypeBound]>`), leaving the stack
   at the mark. No clone of `TypeBound::ForLifetime`/`Use`'s `ThinVec`s.
5. `ExpressionStoreBuilder::set_block_tail(&mut self, block: ExprId, tail: ExprId)`: rewrites
   the tail word in place; panics on a non-block (a lowering bug). `desugar_try_block`.
6. Owned cold values moved into their side table, never cloned from a view (the brief: "build
   it once and move it in"):
   `alloc_path_expr(Path) -> ExprId` (`Expr::Path`), `alloc_path_pat(Path) -> PatId`
   (`Pat::Path`), `alloc_literal_expr(Literal) -> ExprId`, `alloc_offset_of_expr(OffsetOf) ->
   ExprId`, `alloc_asm_expr(InlineAsm) -> ExprId`,
   `alloc_method_call(receiver: ExprId, method_name: Name, args: &[ExprId], generic_args:
   Option<GenericArgs>) -> ExprId`, `alloc_record_lit(path: Path, fields: &[RecordLitField],
   spread: RecordSpread) -> ExprId`, `alloc_record_pat(path: Path, args: &[RecordFieldPat],
   ellipsis: bool) -> PatId`, `alloc_tuple_struct_pat(path: Path, args: &[PatId], ellipsis:
   Option<u32>) -> PatId`. Each is the matching view's `push_expr`/`push_pat` with the owned
   value pushed into `paths`/`literals`/`offset_ofs`/`asms`/`names`/`generic_args` instead of a
   clone; the slices are copied like the view's (lowering passes `lists.*_since(mark)` from a
   taken `Lists`, so no borrow conflict).

`take_lists` while a node is allocated is all lowering does between take and put (nothing
pushes onto the builder's stacks in between). `set_pat`, `exprs_since` etc. on the builder
itself are unused by lowering.

Status: open
