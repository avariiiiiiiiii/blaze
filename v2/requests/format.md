# Requests from lane format

Each: what, where, why. Children of format (format-tree, format-parse, format-span,
format-hir) append below with their name.

## format (lead)

1. **Outline parses for dependency files** — `crates/base-db/src/editioned_file_id.rs`,
   `EditionedFileId::parse` (lane db). Parse files of crates whose origin is not local
   (`CrateOrigin::is_local() == false`, the registry and git dependencies and the sysroot)
   with `ast::SourceFile::parse_outline(text, edition)` and the rest with
   `ast::SourceFile::parse_shared(text, edition)`; `parse_errors` of a dependency file uses
   `Parse::errors_with_bodies()` only when the build checks that crate (`blaze check` checks
   workspace crates fully; a dependency's body that is never reached is never parsed: see the
   report's note on syntax errors in unreached bodies). Why: #1294, docs/v2/plan/lanes/format.md
   "Issues this lane takes in": dependency bodies are parsed only when reached. The parse
   query needs to know the file's crate; if it can't, a second query `parse_outline` beside
   `parse`, chosen by the caller that knows the crate, works as well.

> forwarded to db: `crates/base-db/src/editioned_file_id.rs` does not call `parse_outline` yet.
2. **One text, not two** — `crates/base-db/src/lib.rs:396`, `FileText::text` (lane db):
   make it `std::sync::Arc<str>` instead of `triomphe::Arc<str>`, so `parse_shared` /
   `parse_outline` keep the file's own text in the tree instead of a copy (syntax-tree's
   `FlatTree::source` is a `std::sync::Arc<str>`; it can't take triomphe's without a copy).
   Why: docs/v2/plan/lanes/format.md "Syntax": text read from the source, not copied.

> forwarded to db: `FileText::text` (`crates/base-db/src/lib.rs:709`) still returns triomphe's `Arc<str>`.
3. **Drop the green-tree switch** — `crates/blaze-engine/src/lib.rs:8041` (lane codegen):
   delete `syntax::set_green_trees(std::env::var_os("BLAZE_GREEN_TREES").is_some());`.
   Why: parses make only flat trees now (no-fallback: no second path); `set_green_trees` is
   removed from `syntax`.

> applied: already done in the tree (`rg set_green_trees crates` finds nothing; format-parse's item 2).
4. **Use the item reparse on an edit** — the db's parse query (lane db): when the previous
   parse of the same file is at hand and the VFS gives the edit (`blaze_vfs::Change`), make
   the new parse with `Parse::reparse(delete, insert, edition)` rather than parsing the whole
   file. It gives the same tree as a full parse (format-parse's tests check it). Why:
   docs/v2/plan/lanes/format.md "an edit reparses the changed item".

> forwarded to db
5. **Workspace dependency `blaze-v2-proof`** — root `Cargo.toml` `[workspace.dependencies]`
   (coordinator): `blaze-v2-proof = { path = "crates/blaze-v2-proof" }`, as proof's request 9
   asks. `crates/blaze-format/Cargo.toml` already has `blaze-v2-proof.workspace = true` under
   `[dev-dependencies]` (manifest_lint names it until then). Why: `tiers/tests.rs` runs every
   tier through `columns::round_trips` and `columns::reads_in_place` (docs/v2/plan/design/data.md,
   "Every encoding round-trips exactly, which the proof checks"). The dev-dependency cycle
   (blaze-v2-proof -> blaze-query -> blaze-format) is one cargo allows for dev-dependencies.

> applied: `blaze-v2-proof = { path = "crates/blaze-v2-proof" }` is in the root `[workspace.dependencies]` (ci's request 8 asked the same).

## format-span-intern

`crates/intern/` is now sharded and append-only (docs/v2/plan/lanes/format.md section 5, "IDs, two kinds";
docs/v2/plan/lanes/db.md: memory is bounded by the db's reachability GC rebuilding arenas, not by
refcounts). `Interned`, `InternedRef`, `InternedSlice`, `InternedSliceRef` and `Symbol` are
`Copy` pointers with no reference count; nothing is ever freed. So the GC (`intern::GarbageCollector`,
`GcInternedVisit`, `GcInternedSliceVisit`), `InternedRef::decrement_refcount`,
`InternedSliceRef::decrement_refcount` and the `prevent-gc` feature are gone. These callers
break without the edits below (lane: whoever owns hir-ty / hir; no crate enabled `prevent-gc`,
so no Cargo.toml edit is needed).

1. **Remove the interner GC from the solver** — `crates/hir-ty/src/next_solver/interner.rs`,
   lines 2707 to the end of the file (2818). Old text: everything from
   ```rust
   impl WorldExposer for intern::GarbageCollector {
   ```
   through the end of the file, i.e. that impl, `pub unsafe fn collect_ty_garbage() { ... }`,
   `macro_rules! impl_gc_visit { ... }` with its `impl_gc_visit!( ... );` call, and
   `macro_rules! impl_gc_visit_slice { ... }` with its `impl_gc_visit_slice!( ... );` call.
   New text: nothing (the file ends after `mod tls_cache`'s closing `}` at line 2705).
   Why: the types and the collection no longer exist; values are never freed, so there is
   nothing to mark or sweep. `WorldExposer` itself can stay (every `GenericTypeVisitable`
   impl names it); it now has no implementor, so whoever owns hir-ty may remove it and those
   impls later, but that is not needed to build.

> forwarded to db: `crates/hir-ty/src/next_solver/interner.rs` is db's (db-ty); the GC impl is still at line 2600.
2. **Drop the re-export** — `crates/hir/src/lib.rs:215`. Old text:
   ```rust
           next_solver::{clear_tls_solver_cache, collect_ty_garbage},
   ```
   New text:
   ```rust
           next_solver::clear_tls_solver_cache,
   ```
   Why: `collect_ty_garbage` is removed by request 1 (nothing calls it: `rg collect_ty_garbage`).

> forwarded to db: `crates/hir/src/lib.rs:220` still re-exports it.
3. **`StoredGenericArg` holds no reference count** — `crates/hir-ty/src/next_solver/generic_arg.rs:147-184`.
   Old text:
   ```rust
   #[derive(PartialEq, Eq, Hash)]
   pub struct StoredGenericArg {
       ptr: GenericArgImpl<'static>,
   }

   impl Clone for StoredGenericArg {
       #[inline]
       fn clone(&self) -> Self {
           match self.ptr.kind() {
               GenericArgKind::Lifetime(it) => std::mem::forget(it.interned.to_owned()),
               GenericArgKind::Type(it) => std::mem::forget(it.interned.to_owned()),
               GenericArgKind::Const(it) => std::mem::forget(it.interned.to_owned()),
           }
           Self { ptr: self.ptr }
       }
   }

   impl Drop for StoredGenericArg {
       #[inline]
       fn drop(&mut self) {
           unsafe {
               match self.ptr.kind() {
                   GenericArgKind::Lifetime(it) => it.interned.decrement_refcount(),
                   GenericArgKind::Type(it) => it.interned.decrement_refcount(),
                   GenericArgKind::Const(it) => it.interned.decrement_refcount(),
               }
           }
       }
   }

   impl StoredGenericArg {
       #[inline]
       fn new(value: GenericArg<'_>) -> Self {
           let result = Self { ptr: GenericArgImpl { ptr: value.ptr.ptr, _marker: PhantomData } };
           // Increase refcount.
           std::mem::forget(result.clone());
           result
       }
   ```
   New text:
   ```rust
   #[derive(Clone, PartialEq, Eq, Hash)]
   pub struct StoredGenericArg {
       ptr: GenericArgImpl<'static>,
   }

   impl StoredGenericArg {
       #[inline]
       fn new(value: GenericArg<'_>) -> Self {
           Self { ptr: GenericArgImpl { ptr: value.ptr.ptr, _marker: PhantomData } }
       }
   ```
   Why: `decrement_refcount` is gone (interned values are never freed, so a no-op in its place
   would be a silent path), and the `forget(to_owned())` dance only bumped the count.
   `GenericArgImpl`'s tag bits still work: every interned value sits at its type's alignment
   in the arena, at least 8 for `TyInterned`/`ConstInterned`/`RegionInterned`.

> forwarded to db: `generic_arg.rs` still calls `decrement_refcount` (line 169).

Optional, not needed to build: `impl_internable!(gc; X)` and `impl_slice_internable!(gc; X, H, S)`
are still accepted and mean the same as without `gc;` (`hir-ty/src/next_solver/*.rs`); the
`gc;` can be dropped from those calls whenever someone touches them.

## format-hir

1. **hir-def depends on blaze-format and blaze-ids** — `crates/hir-def/Cargo.toml` (lane db).
   Old text:
   ```toml
   syntax-bridge.workspace = true

   # This is a transitive dep, but we need to specify this
   ```
   New text:
   ```toml
   syntax-bridge.workspace = true
   blaze-format.workspace = true
   blaze-ids.workspace = true

   # This is a transitive dep, but we need to specify this
   ```
   Why: `hir/persist.rs`, `item_tree/persist.rs` and `expr_store/persist.rs` implement
   `blaze_format::Persist` for `ItemTree`, `ExpressionStore`, `Body` and their source maps
   (docs/v2/plan/lanes/format.md section 5). Both crates are already workspace dependencies
   (`Cargo.toml`: `blaze-format = { path = "crates/blaze-format" }`, `blaze-ids = ...`).
   hir-def doesn't build without it.

> forwarded to db: `crates/hir-def/Cargo.toml` is lane db's and names neither yet.

2. **Inner attributes of a lazy body, for every caller** — `crates/syntax/src/ast/traits.rs`,
   `HasAttrs::inner_attributes_node` (format-parse). Today the `ast::Fn` arm
   (`it.body()?.stmt_list()?`) enters the body, so every `attrs_including_inner` /
   `attrs_with_doc_including_inner` of a `fn` parses an outline's lazy body: hir-def's
   `attrs.rs` (`AttrFlags`, docs) does so for every fn it is asked about (lane items' file).
   The item tree no longer goes through it (`item_tree/attrs.rs` `OutlineFn`: when
   `body_is_lazy()`, it reads the body's text and enters the body only when the text after
   `{`, whitespace and comments starts with `#` or a non-ASCII character). The same check
   belongs in the trait, so every caller gets it. Exact edit, in `inner_attributes_node`:
   ```rust
   ast::Fn(it) => it.body()?.stmt_list()?.syntax().clone(),
   ```
   becomes
   ```rust
   ast::Fn(it) => {
       let body = it.body()?;
       if body.syntax().is_lazy() && !may_start_with_inner_attr(body.syntax()) {
           return None;
       }
       body.stmt_list()?.syntax().clone()
   },
   ```
   with `may_start_with_inner_attr` reading `&tree.source()[tree.range(at)]` from
   `node.flat()` as `crates/hir-def/src/item_tree/attrs.rs`'s `may_start_with_inner_attr` and
   `block_comment_len` do (copy them with their tests). Then `OutlineFn` can go: tell
   format-hir and it removes it. Why: #1294, nothing that doesn't lower a body may parse it.

> applied: by format-parse (format-parse's item 1 below).

3. **Test dependence, no edit**: `crates/hir-def/src/expr_store/tests/outline.rs` builds a crate
   with `new_source_root:library` (so `CrateOrigin::Library`) and asserts its file is parsed
   as an outline: it fails, naming base-db's parse query, until the lead's request 1 above is
   applied.

> applied: note only; holds until db applies the lead's request 1.

## format-span

1. **Expect tests that print body ast ids, no edit until built** (hir-def, hir-expand, hir-ide
   tests; owners: format-hir and the coordinator). `span::ast_id` gives ids inside a fn body a
   new layout (`HASH 16 | INDEX 10 | BODY 1 | KIND 5`, the body bit set, the hash bits a slot
   the outline reserves per body, the index counting that kind in the body). Ids outside bodies
   print as before. Any expect test that prints an id of a node inside a fn body (Debug shows
   `Kind[XXXX, n, body]`) changes: rerun it with `UPDATE_EXPECT=1` once built and check only ids
   moved. The marker ids (root, fixup, no-downmap) keep their raw values, so
   `proc-macro-api`'s `OLD_FIXUP_AST_ID` and the proc-macro wire format are unchanged.

> applied: noted for the first compile: hir-ide's expect tests (the coordinator's) are rerun with `UPDATE_EXPECT=1` then, checking only ids moved (in the requests report).
2. **Persisted ast ids** (`hir-def/src/nameres/frozen.rs`, `hir-def/src/hir/persist.rs`,
   `load-cargo`; owners: format-hir, the coordinator): they write `ErasedFileAstId::into_raw`.
   Raw ids from before this change decode to other nodes. If any of these encodings can be read
   back across blaze versions without a version in their cache key, bump it; otherwise nothing.

> forwarded to db: `crates/hir-def/src/nameres/frozen.rs`.
> forwarded to cargo: `load-cargo`'s persisted ids.

## format-parse

1. **`OutlineFn` can go** — `crates/hir-def/src/item_tree/attrs.rs` (format-hir): format-hir's
   request 2 is done. `HasAttrs::inner_attributes_node` (`crates/syntax/src/ast/traits.rs`)
   now returns `None` for a `fn` whose body is lazy and not yet parsed when the body's text
   surely starts with no inner attribute (`block_may_start_with_inner_attr`, the same rule,
   with its tests). Remove `OutlineFn`, its `may_start_with_inner_attr` and
   `block_comment_len` and their tests, and call `ast::Fn`'s `HasAttrs` directly. Why: one
   copy of the rule, for every caller (#1294).

> forwarded to format: `OutlineFn` is still in `crates/hir-def/src/item_tree/attrs.rs:271` (format-hir's).
2. **No edit, already applied**: the lead's request 3 (drop `syntax::set_green_trees` from
   `crates/blaze-engine/src/lib.rs`) is done in the tree (`rg set_green_trees crates` finds
   nothing); `syntax::set_green_trees` no longer exists, so nothing may call it again.

> applied: note only.
3. **An expander returns its errors** — `crates/syntax-tree/src/flat.rs` (format-tree). Make
   `pub type Expand = fn(kind: SyntaxKind, text: &str, data: u32) -> (FlatTree, Vec<(TextRange, Box<str>)>)`,
   the ranges relative to `text`. Keep them with the part, moved to the file like its tree,
   and add `FlatTree::part_errors(&self, at: u32) -> &[(TextRange, Box<str>)]`: lazy node
   `at`'s errors, absolute, parsing the part if it isn't parsed yet. Why (performance): today
   `Expand` returns only the tree, so `Parse::errors_with_bodies` (and `errors()` for the
   bodies parsed so far) finds each body's parser errors by parsing the body a second time
   (`crates/syntax/src/parsing.rs`, `body_parser_errors`). With this each body is parsed once.
   When it lands, tell format-parse: `expand_lazy` returns its parser errors and
   `body_parser_errors` reads `part_errors`. Keep a part's errors in its `heap_size`.

> forwarded to format: format-tree's `crates/syntax-tree/`, inside lane format.
4. **Replace a token in place** — `crates/syntax-tree/src/flat.rs` (format-tree). Add
   `FlatTree::replace_token(&self, at: u32, text: &str, source: Arc<str>) -> FlatTree`: token
   `at` with the new `text` (same kind), over `source`, the new whole text, with every lazy
   node kept lazy and every part already parsed kept (those after `at` moved). Why
   (performance): the token reparse (`crates/syntax/src/parsing/reparsing.rs`, `Edit::token`)
   has to rebuild the token's parent from the arrays to call `replace_subtree`. For whitespace
   or a comment at a file's top level that parent is the root, so a one-character edit rebuilds
   the whole tree and drops every body parsed so far. When it lands, tell format-parse, which
   switches `Edit::token` to it.

> forwarded to format: format-tree's `crates/syntax-tree/`, inside lane format.
