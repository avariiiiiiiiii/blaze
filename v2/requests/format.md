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

Status: done by speed-hir-defs: `blaze-format.workspace = true` added. Not `blaze-ids`: nothing in
hir-def names `blaze_ids` (`rg blaze_ids crates/hir-def` is empty), so it would be an unused
dependency; add it with the first use.

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

## format-tree

1. **Drop the green twin from an outline test** — `crates/syntax-bridge/src/tests.rs`
   (`an_outline_has_the_token_trees_of_a_full_parse`). Green trees are gone from
   `syntax-tree` (format-edit, decisions.md: rowan goes entirely), so `SyntaxNode::new_root`
   and `SyntaxNode::green` no longer exist. Delete line 197,
   `let green = SyntaxNode::new_root(full.green().to_owned());`, and line 222,
   `assert_eq!(token_tree(&green), expected, "{path}");`. Nothing else changes: the test still
   compares the outline with the full parse, which is what it is for (`SyntaxNode` stays
   imported: the test's `in_full` closure uses it). Why: without this, `syntax-bridge`'s
   tests don't build.
   Status: done by speed-span-tt
2. **`expand_lazy` returns its errors** — `crates/syntax/src/parsing.rs` (format-parse).
   format-parse's request 3 has landed in `syntax-tree`: `Expand` is now
   `fn(SyntaxKind, &str, u32) -> (FlatTree, Vec<(TextRange, Box<str>)>)`, and
   `FlatTree::part_errors(at)` gives a lazy node's errors, absolute. `syntax` doesn't build
   until `expand_lazy` matches. The least edit: make it return
   `(FlatTree, Vec<(TextRange, Box<str>)>)`, bind `errors` instead of `_errors` from
   `build_tree`, and end with
   `(tree, errors.into_iter().map(|it| (it.range(), it.to_string().into())).collect())`.
   The ranges must be relative to `text` (the part's own text), which is what `build_tree`
   gives over `Arc::from(text)`. The `ILLEGAL_FLOAT` error, whose position is a token count
   in the file and not an offset, is format-parse's call, as in `body_parser_errors`. Then
   `errors_of` can read `tree.part_errors(at)` (parsing the part if needed) in place of
   `body_parser_errors`.

> forwarded to format: `crates/syntax-bridge/` and `crates/syntax/` are lane format's, between its children.

## From concurrency: lock audit (2026-10-29)

Audited against `docs/v2/plan/design/concurrency.md` by `concurrency-audit-front` (read-only).
Lookups in both interners take no lock, which is right. The findings are on the write side.

1. **`crates/intern/src/arena.rs:26` (`SHARDS = 1 << 6`) and `:47` (`Shard.state: Mutex<State>`).**
   This is the store under every `Interned::new_gc` (hir-ty's `Ty`, `Const`, `Region`,
   predicates; `next_solver/ty.rs:84` and the others) and `Symbol::intern`. That's a hot write path during
   type checking. The shard count is fixed at 64, which breaks "a power of two ≥ 4× the core
   count, chosen at startup". Also, a `Vacant` holder grows the shard's table while holding the
   lock, copying every entry (`table_for_one_more`, :249-277), so the other inserters to that shard
   wait out an O(n) copy. Instead, size `Store.shards` when the store is first made (the stores are
   lazily initialized statics, so `available_parallelism() * 4`, next power of two, is known by
   then) and derive `SHARD_BITS` from that. With more shards each table is smaller, which also
   shortens the copy done under the lock.
   Status: done by speed-span (`crates/intern/src/arena.rs`: shards from the core count, the
   lock a named `InsertLock`; docs/v2/plan/lanes/format/speed-span.md).
2. **`crates/blaze-ids/src/lib.rs:210` (`SHARDS = 64`) and `:224` (`Shard.lock: Mutex<u32>`).**
   It has the same fixed count. On top of that, `Interner::intern` runs the caller's `value()` with
   the shard locked (:340): that's computing under a shard lock, where the rule is "held only to
   claim a slot". Every insert from every shard also writes two process-wide counters,
   `next.fetch_add` (:335) and `published.fetch_add` (:347), so all writers contend on one line.
   Instead: pick the count at startup as in 1. After the lock-free `find` misses, compute `value()`
   *before* locking, then lock only to probe again and insert, dropping the loser's value. Hand out
   indices from per-worker blocks (claim 64 at a time from `next`), and keep `published` per
   shard, summed by `len()`. There are no users outside the crate yet, so now is the cheap time
   to change it.

Judged off the hot path: `syntax-tree/src/flat.rs:1167` (`PARTS`, tests only). The format files
in `hir-def` (`expr_store*`, `hir*`, `item_tree*`) and `syntax`, `span`, `tt` and `mbe` hold no locks.

> forwarded to format: every finding is in lane format's own crates (`intern`, `blaze-ids`, `syntax-tree`, `span`); lane format is still running.

## format-hir: expressions and patterns become columns (to lanes db, items, codegen)

From format-hir, 2026-10-29. Per the lead's addendum and `docs/v2/plan/design/data.md`, the
`ExpressionStore`'s expressions and patterns are now a `u8` kind and a `u32` data column each
(at most 12 bytes per expression), with child lists as CSR ranges and cold values in side
tables. There is no `Arena<Expr>` any more, so `impl Index<ExprId>` / `Index<PatId>` for
`ExpressionStore` are gone. Readers get a borrowed view, `Expr<'a>` / `Pat<'a>`, with today's
variant and field names. Full contract: `~/restart-2026-10-28/briefs/v2/format-hir-columns-api.md`.
About 290 sites by a rough count: hir-ty 72, hir-ty-mir 89, hir-ty-borrowck 87, hir 35,
hir-ty-codegen 6, and 3 in hir-def outside format's paths (`resolver.rs`, `upvars.rs`, ...).

The rule, applied mechanically:
1. For an expression, `store[e]`, `&store[e]`, `body[e]` and `self.body[e]` become
   `store.expr(e)` (`body.expr(e)` through `Deref`). For a pattern, `store.pat(p)`.
2. Fields bind by value for ids, `Option`s of ids and `Copy` enums: `*callee` becomes
   `callee`, and `Some(*e)` becomes `Some(e)`. List fields are `&[T]` as before (`args.iter()`,
   `args.len()` and `&args[..]` are unchanged). `Name`, `Path`, `Literal`, `OffsetOf` and
   `InlineAsm` fields are `&T` as before. `generic_args.as_deref()` becomes `generic_args`,
   which is now an `Option<&GenericArgs>`.
3. A function taking `&Expr` / `&Pat` takes `Expr<'_>` / `Pat<'_>`. `store.exprs()` yields
   `(ExprId, Expr<'_>)`. `Idx<Expr>` written out becomes `ExprId`.
4. Nothing outside hir-def builds an `Expr` or a `Pat`.

Examples from `crates/hir-ty/src/infer/expr.rs`:

```rust
// before (line 336)
let expr = &self.store[tgt_expr];
// after
let expr = self.store.expr(tgt_expr);
```

```rust
// before (line 446)
Expr::Call { callee, args, .. } => self.infer_call(tgt_expr, *callee, args, expected),
// after
Expr::Call { callee, args, .. } => self.infer_call(tgt_expr, callee, args, expected),
```

```rust
// before (line 447)
Expr::MethodCall { receiver, args, method_name, generic_args } => self
    .infer_method_call(tgt_expr, *receiver, args, method_name, generic_args.as_deref(), expected),
// after
Expr::MethodCall { receiver, args, method_name, generic_args } => self
    .infer_method_call(tgt_expr, receiver, args, method_name, generic_args, expected),
```

Owners: lane db (`hir-ty`, `hir-ty-borrowck`, and hir-def's `resolver.rs`/`upvars.rs`), lane
items (`hir`), lane codegen (`hir-ty-mir`, `hir-ty-codegen`). These land together with the
format-hir change. The item tree's own migration follows under "## format-hir-tree".

> forwarded to db and codegen: `hir-ty`, `hir-ty-borrowck`, `hir-def`'s `resolver.rs`/`upvars.rs` and `hir` are lane db's (crates.md gives `crates/hir/` to db, not items); `hir-ty-mir` and `hir-ty-codegen` are codegen's. Lane items is DONE, so its part is `requests`': items' `hir-ty` files have no `store[e]` site; `blaze-items`' four `exprs()` loops (`key.rs:887`, `tests/{form,hashes,facts}.rs`) drop `*closure_kind` for `closure_kind` once `ExpressionStore::exprs` yields `Expr<'_>` (it still yields `&Expr` from the arena).

## speed-types: `ContentId::of_many` and a context key per domain (`crates/blaze-ids/src/lib.rs`)

From `speed-types` (blaze-items' speed pass). `blaze-items` already calls
`ContentId::of_many` (`key.rs` `hash_all`, `index.rs` `name_hashes`, `form.rs`
`Form::finish_many`), so **it does not build until this lands**. The request was in
`v2/requests/items.md` and `store.md` (5) with "forwarded to format", but never here.

1. **`ContentId::of_many(domain: &'static str, inputs: &[&[u8]]) -> Vec<ContentId>`**, equal to
   `inputs.iter().map(|it| ContentId::of(domain, it)).collect()` on every CPU path. The first
   version can be exactly that map over one shared context key (2 below), which already
   removes one compression per input. The SIMD version hashes up to 16 inputs per pass
   (AVX-512; 8 AVX2, 4 NEON/SSE4.1). `blake3::platform::Platform::hash_many` needs every
   input to be the same whole number of blocks, and items' inputs are 40 to 200 bytes of
   different lengths, so it needs a per-lane block length: group by block count, run the
   full blocks through `hash_many`, and finish each lane's last block with its own length. A
   differential test against `ContentId::of` per input, over the empty input, 1, 63, 64, 65,
   1024 and 1025 bytes, mixed in one call.
2. **One context key per domain, not one `new_derive_key` per `Hasher::new`.** `blake3` 1.8.7
   (our lock) has `blake3::hazmat::hash_derive_key_context(context) -> ContextKey` and
   `blake3::hazmat::HasherExt::new_from_context_key(&ContextKey)`. Together they equal
   `Hasher::new_derive_key(context)` bit for bit, with the context's compression done once.
   - `Hasher::new(domain)` looks the key up in a table keyed by the `&'static str`'s address:
     an append-only, open-addressed array of `(ptr, len, key)` slots published with one
     release store (concurrency.md: no lock on a read). Then it calls `new_from_context_key`.
   - Better still, a `pub struct Domain { name: &'static str, key: OnceLock<ContextKey> }`
     made by `const fn Domain::new(name)`, with `Hasher::of(&'static Domain)` and
     `ContentId::of_domain`. Callers' `pub const X_DOMAIN: &str` would become
     `pub static X: Domain`, so the key is one acquire load. Items would move to it in the
     same change.
   - Why: items makes millions of hashes of 40 to 200 bytes (a key per item, two per form, one
     per fact, component and name). Each costs two to four compressions today, and the
     derive-key one is 25 to 50% of the total. A test: `Hasher::new(d)` equals
     `blake3::Hasher::new_derive_key(d)` over several domains.

> forwarded to format: `crates/blaze-ids/src/lib.rs` is lane format's; lane format is still running. `blaze-items` (key.rs, index.rs, form.rs) does not build until `of_many` lands.

## speed-syntax (speed pass of `crates/parser`, `crates/syntax`)

From `speed-syntax`, for format-tree (`crates/syntax-tree/` is live as format-edit). Notes:
`docs/v2/plan/lanes/format/speed-syntax.md`.

Already wired in `crates/syntax` (format-parse is done, its files are speed-syntax's now):
format-tree's request 2 (`expand_lazy` returns its parser errors; `Parse::errors_of` reads
`FlatTree::part_errors` through `parsing::body_errors`, so each body is parsed once and
`body_parser_errors` is gone) and the follow-up of format-parse's request 4 (`Edit::token` calls
`FlatTree::replace_token`).

1. **A sourced token is checked by address, not by its bytes** — `crates/syntax-tree/src/flat.rs`,
   `FlatTreeBuilder::token`. Today a sourced builder compares every token's bytes with the
   source (`source.as_bytes().get(self.next..end) == Some(text.as_bytes())`): a second pass over
   every byte of every file parsed, and the check `parsing::build_tree` can never fail, since its
   tokens are slices of the source it gives the builder. Exact change: accept at once when
   `text.as_ptr() == source.as_ptr().wrapping_add(self.next) && end <= source.len()`, and compare
   the bytes only otherwise (a token copied from another text, as a test builder does); keep the
   panic message. The check is as strong (same address and length is the same bytes) and costs
   two compares per token. Same for `FlatTree::expand`'s check of the part
   (`part.source.get(..) == Some(text)`): with 2 below it is `Arc::ptr_eq(&part.source,
   &self.source) && root == range`, no compare of the body's text.
2. **A part built over the file's own text** — `flat.rs`. Today `expand_lazy` must build each
   body over `Arc::from(text)`: one allocation and a copy of the body per body parsed, then
   `moved()` and the compare above. Exact change: `pub type Expand = fn(kind: SyntaxKind,
   source: &Arc<str>, range: TextRange, data: u32) -> (FlatTree, Vec<(TextRange, Box<str>)>)`,
   errors absolute; `FlatTreeBuilder::sourced_range(source: Arc<str>, range: TextRange)`, a
   sourced builder whose tokens start at `range.start()` (`next = start`, the first mark
   `start`) and whose `finish` checks `next == range.end()` (today `== source.len()`); `expand`
   then calls `(lazy.expand)(kind, &self.source, range, lazy.data)` and keeps the tree as it is,
   with no `moved`. Tell speed-syntax (window `speed-syntax`) when it lands: `expand_lazy`
   moves to `sourced_range` with `LexedStr::new(edition, &source[range])` and absolute errors,
   and the reparse's block, item and body candidates (`reparsing.rs`, `Arc::from(text)` three
   times) build over `new_source` the same way, which removes one copy of the edited node per
   candidate tried.

> applied (format-tree, 2026-10-29): both, as written.
> - 1: a token at the source's next address with `end <= ` the builder's end is accepted
>   without a compare; the message is kept.
> - 2: `Expand` is `fn(SyntaxKind, &Arc<str>, TextRange, u32) -> (FlatTree, Vec<(TextRange,
>   Box<str>)>)`, its errors absolute and checked to lie in the range.
>   `FlatTreeBuilder::sourced_range(source, range)` checks the range is on char boundaries,
>   keeps tokens and lazy nodes inside it, and `finish` checks `next == range.end()`.
>   `sourced` is now `sourced_range` over the whole source. `expand` checks
>   `Arc::ptr_eq(&part.source, &self.source) && root == range` and keeps the part as built,
>   with no `moved`.
> - `syntax` doesn't build until `expand_lazy` takes the new signature (`source[range]` for
>   the lexer, `sourced_range` for the builder). speed-syntax is told.

The lock audit's only note on these files (`flat.rs` `PARTS`) is in tests: nothing to apply.

## speed-hir-expand: span lookups per token, span-map layout, the dropper thread (`crates/span/src/map.rs`, `crates/syntax-bridge/src/lib.rs`)

To `speed-span-ast` (`span`) and `speed-span-tt` (`syntax-bridge`). hir-expand is the caller of all
of these (`crates/hir-expand/src/span_map.rs`, `lib.rs`); I fit my side to whatever you pick, tell
me (`tmux send-keys -t blaze-v2:speed-hir-expand`).

1. **A cursor for in-order lookups.** `syntax_node_to_token_tree*` asks `SpanMapper::span_for` for
   every token of every macro input, in increasing offset order. Both maps answer with a binary
   search per token (`SpanMap::span_at` :123, `RealSpanMap::span_for_range` :359 via `index_at`
   :332, plus `end()` and an `assert!` per call). Wanted, giving exactly the same `Span`s:
   `SpanMap::span_at_from(&self, offset: TextSize, hint: &mut u32) -> Span` and
   `RealSpanMap::span_for_range_from(&self, range: TextRange, hint: &mut u32) -> Span`, which
   start from the entry `hint` names, step forward while the next entry still covers the offset
   (galloping 1, 2, 4, ... then a binary search in the last step, so a jump back or far ahead is
   still `O(log n)`), and store the entry they found in `hint`. Amortized `O(1)` per token
   instead of `O(log n)`, and the entries read are adjacent (one cache line per few tokens instead
   of `log n` scattered ones). Keep `span_at` / `span_for_range` as they are for random lookups.
   hir-expand will then hold a `Cell<u32>` in its mappers (`SpanMap<'_>`, `MacroRulesArgs`), one per
   conversion, so `SpanMapper::span_for(&self, ..)` can stay `&self`; or make it `&mut self` if
   you prefer, and say so.
2. **`SpanMap` as two columns.** Today `spans: Vec<(TextSize, Span)>` (24 bytes an entry: 4 for
   the offset, 20 for the span, with padding). As `ends: Vec<TextSize>` beside `spans: Vec<Span>`, the
   search reads 4-byte offsets only (16 per cache line instead of 2 or 3). `Hash` (salsa
   fingerprints `parse_macro_expansion`'s output, lru = 512, so it is hashed on every execution)
   becomes two slice hashes instead of a derived `Hash` per tuple. Also `with_capacity(tokens)`
   from `token_tree_to_syntax_node`, which knows the token count, instead of growing by `push`.
3. **The dropper thread** (`impl Drop for SpanMap`, :217): each dropped map sends its `Vec`
   through one process-wide `mpsc` channel (a lock per drop) to an OS thread that sleeps 100 ms in
   a loop. Under v2 eviction and quiescent reclamation free memos (concurrency.md, "Memory"), so
   drop the `Drop` impl and the thread; a dropped map frees its columns where it drops.
4. **Down-mapping index.** `ranges_with_span_exact` / `ranges_with_span` (:90, :106) scan every
   entry; `hir`'s `Semantics` descend calls them per token it maps down
   (`crates/hir/src/semantics.rs:1010,1265` through `ExpansionInfo::map_range_down*`). A sorted
   permutation `by_anchor: Vec<u32>` (entry indices ordered by `(anchor, range.start)`) built in
   `finish()` makes each a binary search plus the matching run. Lower priority than 1-3.

## format-hir-encode: untrusted salsa indices in HIR buffers (docs/v2/plan/design/unsafe.md)

From format-hir-encode, for the lead (`crates/blaze-format/`) and format-span (`crates/span/`).

HIR buffers (`expr_store/persist.rs`, `item_tree/persist.rs`, `hir/persist.rs`) store
`SyntaxContext`s as their `into_u32` value and rebuild them with `unsafe
SyntaxContext::from_u32`. That function's precondition is "a valid `SyntaxContext`": a root, or
a salsa index this database interned. Bounds checks can't prove that. Today the `// SAFETY:`
comment rests on "the database that wrote the buffer is the one reading it". A corrupted or
foreign buffer breaks that, and `SyntaxContext::id` then builds a `SyntaxContextImpl` from an
index salsa never made (span/src/hygiene.rs:92). unsafe.md says untrusted bytes are verified
before any view is made of them. Two ways to close it; either is enough:

1. **The lead, `blaze-format`:** make hash verification part of opening a buffer. Add
   `pub fn open_verified<R: Record>(buffer: &[u8], schema: SchemaHash, key: ContentId) ->
   Result<&R, FormatError>`, which checks `ContentId::of(.., buffer) == key` before `open_as`, and
   a `FormatError::Hash` variant. `Persist::read` documents that its caller verified the bytes;
   the daemon image and the store only call `read` on verified bytes. Then the HIR `SAFETY:`
   comments can name that check as where the invariant is upheld.
2. **format-span, `span`:** a safe, checked constructor, `pub fn
   SyntaxContext::from_u32_in(db: &dyn salsa::Database, raw: u32) -> Option<SyntaxContext>`.
   It returns `None` unless `raw` is a root id or an index the interned `SyntaxContextImpl`
   table holds. Then the decoders take the database and need no `unsafe` at all.

Until one lands, the only `unsafe` left for this is the single function
`hir::persist::syntax_context` (format-hir's file; edit given in
/tmp/blaze-lanes/v2-format-hir-encode.md).
