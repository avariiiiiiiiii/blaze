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
6. **Pin blake3 exactly** — root `Cargo.toml` `[workspace.dependencies]` (coordinator):
   `blake3 = { version = "=1.8.7", default-features = false, features = ["std", "neon"] }`
   in place of `"1.8"`. Why: `ContentId::of_many` (`crates/blaze-ids/src/many.rs`) drives
   `blake3::platform::Platform::hash_many`, which blake3 marks unstable, to hash many short
   inputs a SIMD lane each (docs/v2/plan/lanes/format/speed-ids-format.md). A test holds it to
   `blake3::Hasher` at every length, so a bump is a deliberate change that runs that test,
   not a lockfile refresh.
7. **Warm words are `u32` now** — `crates/blaze-cas/src/tests.rs:1000`, `OneColumn::hot_from_warm`
   (lane store): read the words as `u32`s,
   `warm[WARM_HEAD..].chunks_exact(4).map(|it| u32::from_le_bytes(it.try_into().unwrap()))`,
   in place of `chunks_exact(8)` and `u64::from_le_bytes`. `tiers::Packed::words` is a
   `Vec<u32>` in SIMD-BP128 layout. The writer at :992 already writes them as 4 bytes each.
   Why: the warm tier's speed pass (docs/v2/plan/lanes/format/speed-ids-format.md); until this
   changes, blaze-cas's tests don't compile.

**Speed pass (2026-10-29), changes others see** (docs/v2/plan/lanes/format/speed-ids-format.md):
- `blaze-format`: `Record` has `const PLAIN: bool` (default `false`). `record!` sets it when
  every field is plain, and a plain record is checked by bounds alone. A hand-written
  `unsafe impl Record` may set it only if every bit pattern of its size is a valid value.
- `blaze-format::tiers`: `WarmColumn` holds `u32` words in SIMD-BP128 layout (its schema hash
  changed), and cold columns are `COLD_VERSION` 2. No buffer of either was ever stored, so
  nothing needs to migrate. `ColdDictionary` keeps prepared zstd dictionaries: it clones by
  re-preparing and compares by `id`.
- `blaze-ids`: `ContentId::of_many` hashes one-chunk inputs through `hash_many`, with the
  same results. `Sharded`'s table places are tagged `u64`s, and `find` on an empty shard no
  longer allocates. The API is unchanged.

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
   Status: done by format (lead). `shard_count()`: `(cores * 4).next_power_of_two().max(4)`,
   chosen once; the shard is picked by the ID's first 4 bytes. `intern` computes `value()`
   before locking and locks only to probe again and insert (a racing loser's value is dropped
   after the lock is released), so a `value` may now intern into the same interner. Each shard
   claims indices `BLOCK` (64) at a time from `next` under its own lock; `len` is per shard
   (`#[repr(align(128))]` shards), summed by `len()`. New `Sharded::end()`: above every index
   handed out, by at most `BLOCK` per shard (size `Idx`-indexed columns by it, not by `len()`;
   no current user sizes by `len()`). `pub const SHARDS` is gone (no user outside the crate).

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

- `resolver.rs`: done by speed-hir-nameres-paths. It has no `ExprId`/`PatId` index site: its
  store reads are `impl_data.store[target_trait.path]` (a `PathId`) and
  `ExpressionStore::of(db, owner)[label]` (a `LabelId`), which keep `Index` under the contract,
  so nothing changes.

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

Status: done by format (lead), in `crates/blaze-ids/src/lib.rs`:
- `ContentId::of_many(domain: &'static str, inputs: &[&[u8]]) -> Vec<ContentId>`: the domain's
  context key looked up once, then one `new_from_context_key` hasher per input. Not the
  `hash_many` SIMD batch: `blake3::platform` is `#[doc(hidden)]`, "undocumented and unstable,
  for benchmarks only", so any blake3 bump may break or silently change it; the context key
  (stable `blake3::hazmat`) already removes the derive-key compression, the 25-50% named.
- `Hasher::new(domain)` reads the key from a 1024-slot open-addressed table keyed by the name's
  text (not its address: a `const &str` may sit at several addresses), each slot a `OnceLock`
  (a read is acquire loads; more than 1024 names panics, naming the constant).
- `pub struct Domain`, `const fn Domain::new(name)`, `Domain::name()`, `Hasher::of(&Domain)`,
  `ContentId::of_domain(&Domain, bytes)`: the key in the `Domain` itself, one acquire load.
  Items may move its hot domains to `pub static X: Domain`; nothing requires it.
- Tests: `Hasher::new(d)` equals `blake3::Hasher::new_derive_key(d)` over several domains and
  lengths, `of_many` equals `of` per input over 0, 1, 63, 64, 65, 1024, 1025 and 3000 bytes
  mixed, 600 domains from 4 threads, the same name at two addresses.

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

> forwarded to format (`span`, `syntax-bridge`) and to speed (speed-span-ast, speed-span-tt), both still running.

Status (syntax-bridge side) done by speed-span-tt. `SpanMapper::span_for` stays `&self`: hold the
`Cell<u32>` hint in hir-expand's mappers, one per conversion. Within one
`syntax_node_to_token_tree*` call, `span_for` and `fragment_bounds` see ranges in increasing
start order: one call per token, one per char of a multi-char punct, and two for a lifetime, the
`'` then the rest. A doc comment's span and a closing delimiter's span are asked at that token's
own range. Leaves that `on_enter`/`append` inject are the exception. Only the `_modified` entry
point takes them, and each comes with the empty range at 0: `fragment_bounds` is asked about that
range, `span_for` is not. A hint survives the jump back, because any hint gives the same answer.
Apart from that, `span_at_from` and
`span_for_range_from` give amortized two compares per token. For item 2, `token_tree_to_syntax_node`
now builds its map with `SpanMap::with_capacity(parser_input.len())`.

Status (span side) done by speed-span-ast, checked by speed-span (`crates/span/src/map.rs`):
1. `SpanMap::span_at_from(offset, &mut u32) -> Span` and
   `RealSpanMap::span_for_range_from(range, &mut u32) -> Span` (salsa feature, as
   `span_for_range`): a galloping search from the hint, the same `Span` as `span_at` /
   `span_for_range`, which stay. `&self`, hint a `Cell<u32>` per mapper starting at 0 (as
   `hir-expand/src/span_map.rs` and `eager.rs` already do).
2. `SpanMap` is `ends: Vec<TextSize>` beside `spans: Vec<Span>`; `Hash` is two slice hashes;
   `SpanMap::with_capacity(n)`.
3. `impl Drop` and the dropper thread are gone, from `SpanMap` and from `AstIdMap`.
4. `by_anchor`: a lazy `OnceLock<Box<[u32]>>` permutation by `(anchor, range)`, reset by `push`
   and `merge` instead of built in `finish()`; `ranges_with_span_exact` / `ranges_with_span` are
   binary searches plus the run, same results in the same text order. hir-expand's doc comments
   calling them a linear search (`lib.rs` near :1333 and :1354) are now out of date.

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

Status: 1 done by format (lead), in `crates/blaze-format/src/lib.rs`: `BUFFER_DOMAIN`
(`"blaze v2 buffer 1"`), `buffer_id(buffer) -> ContentId` (the whole buffer, header included),
`open_verified::<R>(buffer, schema, key)`, `read_verified::<P>(buffer, key)` and
`FormatError::Hash { found, wanted }`; `Persist::read`'s doc says bytes this process did not
write go through `read_verified`. For lanes store and db (they read buffers back): key stored
HIR buffers by `blaze_format::buffer_id` and open them with `read_verified`. The `SAFETY:`
comment in `hir::persist::syntax_context` can now name `read_verified` as where the invariant
is upheld (format-hir told).

> forwarded to format: `crates/blaze-format/` and `crates/span/` are lane format's.

## speed-span-tt: an in-order token walk of a flat tree (`crates/syntax-tree/src/flat.rs`)

From `speed-span-tt`, for format-tree (live as format-edit). Notes:
`docs/v2/plan/lanes/format/speed-span-tt.md`.

`syntax_node_to_token_tree` on a flat node, through syntax-bridge's `FlatTokens`, reads every
token of a macro input in order through `FlatTree::range(at)`. `range` computes `offset(at)` from
the last mark, which is up to `MARK` (8) `text_len` steps per token. That is the bridge's only
per-token cost above O(1). Before this change it called `range` and then `token_text`, so the
cost was paid twice; now it is paid once, with the text sliced from `text()`.

1. **`FlatTree::tokens_in(&self, at: u32) -> impl Iterator<Item = (u32, TextRange)>`**: the tokens
   and lazy nodes from `at` to `end(at)` in order, each with its range. It starts from
   `offset(at)` once, then adds `text_len` as it goes, so each step is O(1). A lazy node gets its
   range, and its text is skipped. Same ranges as `range(it)` for each `it`. Or make `text_len`
   public, and the bridge keeps the running offset itself. Tell `speed-span-tt` or whoever owns
   `crates/syntax-bridge/` then; `FlatTokens::next` is the only caller to change.

> forwarded to format: `crates/syntax-tree/` is lane format's.

## speed-hir-store-readers: the lazy nodes as a slice (`crates/syntax-tree/src/flat.rs`)

From `speed-hir-store-readers` (owns `crates/hir-def/src/expr_store/body.rs`), for whoever owns
`crates/syntax-tree/` (speed-syntax / format-tree).

`body_syntax` asks, per anchor of a file, whether its subtree holds a lazy node of an outline
(`holds_lazy` in `expr_store/body.rs`): `tree.lazies().find(|&it| it >= at)`. `lazies()` is an
iterator over `Lazy::nodes` (sorted, preorder), so each anchor scans every lazy node before it:
O(anchors x bodies) for every edit of a dependency's file, where a binary search is O(log bodies).

1. **`FlatTree::lazy_nodes(&self) -> &[u32]`**: the lazy nodes in preorder, sorted ascending
   (`self.lazy.as_ref().map_or(&[], |it| &it.nodes)`). `lazies()` can stay as
   `self.lazy_nodes().iter().copied()`.

Once it exists, `holds_lazy` becomes (I make the edit; tell `speed-hir-store-readers` or
`speed-hir-store`):
```rust
let lazies = tree.lazy_nodes();
lazies.get(lazies.partition_point(|&it| it < at)).is_some_and(|&it| it < tree.end(at))
```

> applied (speed-syntax, 10-29): `FlatTree::lazy_nodes(&self) -> &[u32]` in
> `crates/syntax-tree/src/flat.rs`, ascending preorder, empty without lazy nodes; `lazies()` is
> now `self.lazy_nodes().iter().copied()`. Not compiled (no builds in this lane).

## format-hir-tree

From format-hir-tree (a child of format-hir), 2026-10-29. Not compiled (this lane runs no
builds). Per `docs/v2/plan/design/data.md` and the lead's addendum, the `ItemTree`
(`crates/hir-def/src/item_tree.rs`) is now columns:
- An item is a dense `ModItemId` into three per-item columns: kind (`u8`), data word (`u32`) and
  `FileAstId` (`u32`). That is `ITEM_BYTES = 9` per item, const-asserted.
- Each kind's payload is a flat column of plain `u32` records (at most 16 bytes: `Struct` and
  `Mod`).
- Lists are `IdRange`s: inline module and extern block items, a `use` tree's children, a tuple
  struct's constructor visibilities.
- Names and paths are per-tree ids (`NameId`, `PathId`). Aliases are `AliasId`, use trees
  `UseTreeId`.
- Attributes are one flat array, one `IdRange` per item, read as `AttrsOrCfgRef<'_>`.

`ItemTree::index(id)` still gives `ModItemKind::X(ast_id, &X)`. What changed is what the record
fields hold. These sites are lanes db's and items' (`nameres/`, `src.rs`); every one is in
`crates/hir-def/src/nameres/collector.rs` except the last.

The rule:
1. A record's `name` is a `NameId`. `&it.name` becomes `&item_tree[it.name]` (or
   `&self.item_tree[it.name]`). `it.name.clone()` becomes `item_tree[it.name].clone()`.
   `Const::name` is `NameId::NONE` for `const _`; read it as `item_tree.get_name(it.name)`, an
   `Option<&Name>`.
2. `MacroCall::path` is a `PathId`, so `&item_tree[path]` is the `Interned<ModPath>`.
   `ExternCrate::alias` is an `AliasId`, so `item_tree.import_alias(alias)` is the
   `Option<ImportAlias>`.
3. Lists come from the tree:
   - `&block.children` becomes `item_tree.items(block.children)`.
   - `match &module.kind` becomes `match item_tree.mod_kind(module)`. That gives
     `ModKind::Inline { items: &[ModItemId] }` or `ModKind::Outline`.
   - `match &it.value_ns_ctor` becomes `match item_tree.value_ns_ctor(it)`. The variants keep
     their names: `ValueNsCtorWithVis(&RawVisibilityId)`, `ValueNsCtorWithMinVis(&[RawVisibilityId])`.
4. Attributes: `item_tree.attrs(id)` and `top_level_attrs()` give `Option<AttrsOrCfgRef<'_>>`,
   a `Copy` enum with two variants:
   - `Enabled { attrs: Attrs<'_> }`;
   - `CfgDisabled(&CfgExpr, Attrs<'_>)`.

   So `attrs.as_ref()` becomes `attrs`, `cfg.0` / `&cfg.0` becomes `cfg`, and `it.1.as_ref()`
   becomes the second field. `AttrsOrCfg::lower` (owned, used by `nameres/assoc.rs`) is
   unchanged.
5. `Use::expand(&self, cb)` becomes `Use::expand(&self, tree: &ItemTree, cb)`.
   `lower_use_tree(..)` returns `Option<Arena<ast::UseTree>>`, the source map alone.

Examples:

```rust
// before (collector.rs 2962)
update_def(self.def_collector, fn_id.into(), &it.name, vis, None);
// after
update_def(self.def_collector, fn_id.into(), &item_tree[it.name], vis, None);
```

```rust
// before (collector.rs 2815-2822)
Some(AttrsOrCfg::Enabled { attrs }) => attrs.as_ref(),
None => Attrs::EMPTY,
Some(AttrsOrCfg::CfgDisabled(cfg)) => {
    let ast_id = item_id.ast_id(self.item_tree).erase();
    self.emit_unconfigured_diagnostic(InFile::new(self.file_id(), ast_id), &cfg.0);
// after
Some(AttrsOrCfgRef::Enabled { attrs }) => attrs,
None => Attrs::EMPTY,
Some(AttrsOrCfgRef::CfgDisabled(cfg, _)) => {
    let ast_id = item_id.ast_id(self.item_tree).erase();
    self.emit_unconfigured_diagnostic(InFile::new(self.file_id(), ast_id), cfg);
```

```rust
// before (collector.rs 2936)
self.collect(&block.children, ItemContainerId::ExternBlockId(extern_block_id))
// after
self.collect(item_tree.items(block.children), ItemContainerId::ExternBlockId(extern_block_id))
```

Exact edits, `crates/hir-def/src/nameres/collector.rs` (line numbers as of this note):
- 44: in the `item_tree::{..}` import, `AttrsOrCfg` becomes `AttrsOrCfgRef`.
- 190: `item.expand(|idx, path, kind, alias| {` becomes
  `item.expand(tree, |idx, path, kind, alias| {`.
- 858-862 (`seed_with_top_level`):
  ```rust
  let attrs = match item_tree.top_level_attrs() {
      Some(AttrsOrCfgRef::Enabled { attrs }) => attrs,
      None => Attrs::EMPTY,
      Some(AttrsOrCfgRef::CfgDisabled(_, attrs)) => attrs,
  };
  ```
- 956-957: the condition becomes
  `if let Some(AttrsOrCfgRef::CfgDisabled(cfg_expr, _)) = item_tree.top_level_attrs() {`.
  Delete the next line, `let (cfg_expr, _) = &**attrs;` (`cfg_expr.clone()` at 961 stays).
- 972: `ModItemKind::Mod(_, module) if module.name.as_str() == name)` becomes
  `ModItemKind::Mod(_, module) if item_tree[module.name].as_str() == name)`.
- 998: `Some(AttrsOrCfg::Enabled { .. }) | None` becomes `Some(AttrsOrCfgRef::Enabled { .. }) | None`.
- 2815-2822: as in the second example above.
- 2852, after `let item_tree::ExternCrate { name, visibility, alias } = item;`, add:
  ```rust
  let name = &item_tree[*name];
  let alias = item_tree.import_alias(*alias);
  ```
  Then 2874 `let name = match alias {` becomes `let name = match &alias {`. The arms and the
  uses of `name` after it stay as they are: `*name == sym::self_`,
  `.get(name)`, `name.cloned()`.
- 2936: as in the third example above.
- `&it.name` becomes `&item_tree[it.name]` at 2962 and 2967 (`parse_proc_macro_decl`), 3010,
  3029, 3046, 3074, 3086 and 3098.
- 2982: `match &it.value_ns_ctor {` becomes `match item_tree.value_ns_ctor(it) {`. The arms stay.
- 3053: `match &it.name {` becomes `match item_tree.get_name(it.name) {`. The arms stay.
- `collect_module` (3158-3258):
  - At the top, add `let item_tree = self.item_tree; let name = &item_tree[module.name];`.
  - 3166: `match &module.kind {` becomes `match item_tree.mod_kind(module) {`.
  - `module.name.clone()` becomes `name.clone()` at 3170, 3223 and 3247.
  - `&module.name` becomes `name` at 3177 and 3206.
  - 3215-3218: `Some(AttrsOrCfgRef::CfgDisabled(cfg, _)) => {`, then pass `cfg` for `&cfg.0`.
  - 3221: `Some(AttrsOrCfgRef::Enabled { .. }) | None => {`.
  - 3238: `matches!(top_level_attrs, Some(AttrsOrCfgRef::Enabled { attrs }) if attrs.by_key(sym::macro_use).exists())`.
- `collect_macro_rules`: 3417 `None => &mac.name,` becomes `None => &self.item_tree[mac.name],`.
  3461 `mac.name.clone(),` becomes `self.item_tree[mac.name].clone(),`.
- `collect_macro_def`:
  - At the top (after `let krate = ..`), add
    `let item_tree = self.item_tree; let mac_name = &item_tree[mac.name];`.
  - `&mac.name` becomes `mac_name` at 3480, 3485 and 3504.
  - 3495: `name == mac.name` becomes `name == *mac_name`.
  - 3497: `mac.name.display(..)` becomes `mac_name.display(..)`.
  - 3532: `mac.name.clone()` becomes `mac_name.clone()`.
- `collect_macro_call`: 3552 `let &MacroCall { ref path, expand_to, ctxt } = macro_call;`
  becomes:
  ```rust
  let &MacroCall { path, expand_to, ctxt } = macro_call;
  let path = &self.item_tree[path];
  ```
  The `path.clone()` at 3553 is unchanged and still gives an `Interned<ModPath>`.

Status (`nameres/collector.rs`, every edit above): done by speed-hir-nameres-fixpoint. `src.rs` is not
its path.

`crates/hir-def/src/src.rs` 59-63 (`use_tree_source_map`): delete the `.1` after
`.expect("failed to lower use tree")`, because `lower_use_tree` returns the arena itself.

One more, for whoever owns `crates/span`: `FileAstId::downcast_unchecked` debug-asserts the
id's kind, and `ErasedFileAstId::kind_typed` / `ErasedFileAstIdKind::syntax_kind` are private.
So a decoded item tree can't check that each item's ast id has its kind's syntax kind, and a
corrupted buffer panics in debug builds at `ItemTree::index` where it should be refused.
Wanted: `pub fn ErasedFileAstId::may_be<N: AstIdNode>(self) -> bool`, the same test the
debug assertion makes. `item_tree/persist.rs`'s `check` would then call it per item (I'd make
that edit).

> forwarded to format: between its own children.

## format-hir: type refs are columns too (addition to the rule above; to lanes db, items)

From format-hir, 2026-10-29. `hir/type_ref.rs` keeps a store's type refs as columns
(`TypeColumns`: a u8 kind and a u32 word each, 5 bytes against a budget of 12, const-asserted).
`TypeRef<'a>` is a view, `TypeRefId = Idx<TypeRef<'static>>`, and `Index<TypeRefId>` for
`ExpressionStore` is gone. The same rule applies:
1. `store[t]` / `&store[t]` becomes `store.type_ref(t)`. `store.types.iter()` (or an
   iteration over every type ref) becomes `store.type_refs()`, yielding
   `(TypeRefId, TypeRef<'_>)`.
2. Fields of the view: `Tuple(&[TypeRefId])`, `ImplTrait(&[TypeBound])` and
   `DynTrait(&[TypeBound])` are slices as before. `Path(&Path)`, `Reference(&RefType)`,
   `Array(&ArrayType)` and `Fn(&FnType)` are references (an earlier `&**fn_type` becomes
   `fn_type`). `RawPtr(TypeRefId, Mutability)`, `Slice(TypeRefId)`,
   `TypeParam(TypeParamId)` and `PatternType(TypeRefId, PatId)` bind by value
   (`*inner` becomes `inner`).
3. A function taking `&TypeRef` takes `TypeRef<'_>`.

Sites, about 73: hir-ty `lower.rs` 27, `lower/path.rs` 4, `generics.rs` 2 (lane db);
hir-ty-borrowck `regions.rs` 30 (lane db); hir `display.rs` 6, `diagnostics.rs` 2,
`semantics.rs` 1, `lib.rs` 1 (lane items). These land with the expression change.

> applied (hir-ide): `display.rs`'s five type-ref reads (`TypeRefId::hir_fmt`, its where-clause and return-type checks, the paren-sugar tuple and return) are `store.type_ref(t)` with view fields by value. forwarded to db: the hir-ty, hir-ty-borrowck and `hir` sites (`crates/hir/` is lane db's in `crates.md`, not items').

## From safety-audit

From `safety-audit-front` (child of `safety-audit`, lane `safety`). The workspace is about to deny
`unsafe_op_in_unsafe_fn`, `clippy::undocumented_unsafe_blocks` and
`clippy::multiple_unsafe_ops_per_block`. Every file below was BUSY, so these are for you to paste.
Comments, `# Safety` sections, block splits and crate-root attributes only: no behavior change.
A split keeps the order of evaluation and binds only `Copy` values. Line numbers are as of
2026-10-29 18:45.

> forwarded to format: every crate named is its.

### intern

Status: done by speed-span for `arena.rs`, `intern.rs` and `intern_slice.rs` (as proposed;
`arena.rs:135`'s `Table::new` is now normal-build only, beside a model-build twin). The two
`symbol.rs` items are adapted to its current `Text` and handed to fix-orderkeys, which is
editing that file (`v2/requests/speed.md`, "speed-span" item 2). `mbe` now has
`#![forbid(unsafe_code)]` (listed under "Left for lanes" in the audit report).

- `crates/intern/src/arena.rs:135` block (`Table::new`): 2 ops (two `zeroed` calls). Split:
  ```rust
  // SAFETY: all-zero bytes are a `Group`: a zero control word (every slot empty) and null pointers.
  let slots: Box<[Group]> = unsafe { zeroed(groups) };
  // SAFETY: all-zero bytes are an `AtomicU64` of 0.
  let hashes: Box<[AtomicU64]> = unsafe { zeroed(groups * GROUP) };
  Table { groups: slots, hashes }
  ```
- `crates/intern/src/arena.rs:412` block (`Bump::alloc`): 3 ops (`cursor.add`, `at.add`, `NonNull::new_unchecked`). Split:
  ```rust
  // SAFETY: `pad + size <= left`, the bytes of the current chunk from `cursor`, so `cursor + pad` is in the chunk.
  let at = unsafe { self.cursor.add(pad) };
  // SAFETY: as for `at`: `at + size` is at most one past the chunk's last byte.
  self.cursor = unsafe { at.add(size) };
  self.left -= pad + size;
  // SAFETY: `at` is inside a chunk from `chunk`, an allocation `own` never returns null.
  unsafe { NonNull::new_unchecked(at) }
  ```
- `crates/intern/src/arena.rs:464` block (test helper `intern`, `to.cast::<u64>().write(value)`): no SAFETY. Put the comment on its own line between `Entry::Vacant(vacant) => vacant` and `.insert(..)`: `` // SAFETY: `insert` hands `write` fresh memory of `Layout::new::<u64>()`, aligned and written by no one else. ``
- `crates/intern/src/intern.rs:148` `unsafe impl<T: Sync> Sync for InternedRef<'_, T>`: no SAFETY (the comment above `:147` covers only the `Send` impl). Proposed: `` // SAFETY: as for `Send`: sharing an `InternedRef` shares only a `&T` to a value that lives for the process, which `T: Sync` makes safe. ``
- `crates/intern/src/intern_slice.rs:54` `unsafe impl Sync for InternedSlice<T>`: no SAFETY of its own. Proposed: `` // SAFETY: as for `Send`: an `InternedSlice` is a shared reference to a header and items that are `Send + Sync`, live for the process and are never written again. ``
- `crates/intern/src/intern_slice.rs:107` block (`write`): 4 ops (two derefs of `fat`, `write`, `copy_from_nonoverlapping`). Split:
  ```rust
  // SAFETY: `fat` points at writable memory of `layout::<T>(slice.len())` (the precondition), so its `header` field is in bounds and aligned (`repr(C)`); this takes the field's address without reading it.
  let header_at = unsafe { &raw mut (*fat).header };
  // SAFETY: `header_at` is in bounds, aligned and not initialized yet; `write` drops nothing.
  unsafe { header_at.write(HeaderWithLength { header, length: slice.len() }) };
  // SAFETY: as for `header_at`: the address of the `slice` field, `slice.len()` items long.
  let items_at = unsafe { &raw mut (*fat).slice }.cast::<T::SliceType>();
  // SAFETY: `items_at` has room for `slice.len()` items and is aligned for them, and it is fresh memory, so it cannot overlap `slice`; `SliceType: Copy`, so copying the bytes is a copy.
  unsafe { items_at.copy_from_nonoverlapping(slice.as_ptr(), slice.len()) };
  ```
- `crates/intern/src/intern_slice.rs:122` block (`pointee`): 3 ops (deref of `header`, `read`, deref of the fat pointer). Split:
  ```rust
  let header = at.cast::<HeaderWithLength<T::Header>>().as_ptr();
  // SAFETY: `at` starts a published `Pointee<T>` (the precondition), which starts with its initialized `HeaderWithLength`; this takes the `length` field's address.
  let length_at = unsafe { &raw const (*header).length };
  // SAFETY: `length_at` is the initialized `length` of a value never written again.
  let length = unsafe { length_at.read() };
  // SAFETY: `length` is the value's own slice length, so the fat pointer covers exactly the value, which lives for the process.
  unsafe { &*fat::<T>(at, length) }
  ```
- `crates/intern/src/intern_slice.rs:188` `unsafe impl Sync for InternedSliceRef<'_, T>`: no SAFETY of its own. Proposed: `` // SAFETY: as for `Send`: a shared reference to `Send + Sync` values that live for the process. ``
- `crates/intern/src/symbol.rs:33` `unsafe impl Sync for Symbol`: no SAFETY of its own. Proposed: `` // SAFETY: as for `Send`: a `Symbol` is a shared reference to a `&'static str` that is never written again. ``
- `crates/intern/src/symbol.rs:86` block (`Symbol::intern`, the `insert` closure): 5 ops (`add`, `copy_from_nonoverlapping`, `from_raw_parts`, `from_utf8_unchecked`, `write`). Split:
  ```rust
  // SAFETY: `to` is fresh memory of `layout`, and `bytes_at` is where `Layout::extend` put the bytes in it, so the pointer stays in the allocation.
  let bytes = unsafe { to.as_ptr().add(bytes_at) };
  // SAFETY: `bytes` has room for `s.len()` bytes (the array part of `layout`) and is fresh memory, so it cannot overlap `s`.
  unsafe { bytes.copy_from_nonoverlapping(s.as_ptr(), s.len()) };
  // SAFETY: the `s.len()` bytes at `bytes` were just written, and the arena never moves, writes or frees them, so they live for `'static`.
  let bytes: &'static [u8] = unsafe { std::slice::from_raw_parts(bytes, s.len()) };
  // SAFETY: `bytes` is a copy of `s`, a `&str`, so it is UTF-8.
  let text: &'static str = unsafe { std::str::from_utf8_unchecked(bytes) };
  // SAFETY: `layout` starts with a `&'static str` (`Layout::new`), so `to` is aligned for it and has room, and nothing was there to drop.
  unsafe { to.cast::<&'static str>().write(text) };
  ```
- Everything else in `intern` (`arena.rs:128`, `:240`, `:295`, `:387`, `:431`, `:445`, `:457`; `intern.rs`; `intern_slice.rs:66`, `:75`, `:152`, `:199`; `symbol.rs:59`, `:69`, `:75`, `:106`, `:113`): ok.

### syntax-tree

`crates/syntax-tree/Cargo.toml` has no `[lints] workspace = true`, so the lints don't bind it until it does; the findings stand either way.

- `crates/syntax-tree/src/cursor.rs:186` unsafe fn `flat_unbound`: its contract is a `SAFETY:` paragraph, not a `# Safety` section. Proposed doc:
  ```rust
  /// [`flat`] for a root that is not borrowed from a cursor.
  ///
  /// # Safety
  ///
  /// The caller holds a cursor into `root`'s file, which keeps `root` alive (a part's root lives
  /// as long as its file's), for as long as `'a`.
  ```
- `crates/syntax-tree/src/cursor.rs:187` block (`&*root.as_ptr()`): no SAFETY. Proposed: `` // SAFETY: the caller's precondition keeps `root` alive for `'a`: a `FlatRoot` made from a box by `new_flat_root` or `part_root`, freed only when the last cursor into its file drops, and only ever read through shared references. ``

### syntax

- `crates/syntax/examples/trees.rs:29` block (`System.alloc`): the comment `as the system allocator's own` names no invariant. Proposed: `` // SAFETY: the caller upholds `GlobalAlloc::alloc`'s contract (a non-zero-sized `layout`), which is `System`'s too. ``
- `crates/syntax/examples/trees.rs:34` block (`System.dealloc`): same. Proposed: `` // SAFETY: `ptr` came from `alloc` above with `layout`, which is `System.alloc`'s, so it is `System`'s to free with that layout. ``
- `src/raw.rs` and the crate root (`deny(unsafe_code)` with `#[allow(unsafe_code)] mod raw`): ok.
- `crates/syntax/fuzz/` is a package of its own (its own `[workspace]`, no workspace lints) with no unsafe; `#![forbid(unsafe_code)]` at the top of `fuzz_targets/parser.rs` and `reparse.rs` is optional, since libfuzzer's `fuzz_target!` expands outside them.

### blaze-ids, mbe (item 4)

- `crates/blaze-ids/src/lib.rs:12` crate: no `unsafe`. Add `#![forbid(unsafe_code)]` after the `//!` docs.
- `crates/mbe/src/lib.rs:9` crate: no `unsafe` (`parser.rs:21`'s `sym::unsafe_` is a symbol). Add `#![forbid(unsafe_code)]` after the `#![cfg_attr(feature = "in-rust-tree", ...)]` line.

### Checked, nothing to do

- `tt` (`lib.rs:7`), `syntax-bridge` (`lib.rs:4`) and `const-ast` (`lib.rs:28`) already forbid `unsafe_code`.
- `blaze-format`: `Record`'s `# Safety` (`lib.rs:48`) asks for `#[repr(C)]` or a primitive, no padding, no pointer, and every bit pattern `check` accepts being a value. Every impl meets it: `plain!` (primitives, and `ContentId`, `Fingerprint`, `SchemaHash`, each `#[repr(transparent)]` over a byte array or an integer), `bool` (`check` accepts only 0 and 1), `[T; N]`, `Rel<T>` and `Slice<T>` (`#[repr(C)]` integers and a ZST), `SpanRef` (`#[repr(C)]`, size asserted 36), and `record!` (asserts no padding; `SAFETY` written inside the macro body). Its blocks are single-op and commented. The same holds for `blaze-query/src/tests.rs:27`'s `Word`.
- `parser` (`syntax_kind.rs:20`, `lexed_str/scan.rs`), `span` (`hygiene.rs`) and format's `hir-def` files (`hir/persist.rs:615`, `expr_store/lower.rs:2808`, `:3085`, `item_tree/persist.rs:122`; `item_tree.rs` has no unsafe of its own left, using `record!`): ok.
- `#[salsa::interned(unsafe(no_lifetime))]` (a salsa option, not Rust `unsafe`): `span/src/hygiene.rs:62`.

## From safety-fuzz-macrowasm (2026-10-29): `tt::Literal::new` panics on a suffix over 255 bytes

`crates/tt/src/lib.rs:408` (and `:467`): `suffix_len: suffix.len().try_into().unwrap()` with
`suffix_len: u8`. Any literal with a suffix of 256+ bytes panics. rustc lexes it, then reports
an invalid suffix. It is reached from blaze-macro-wasm's lexer and wire reader, with any text
`1` followed by 256 letters (usercode is told, item 6 of its request). Source code through
syntax-bridge may reach it too. Input: `fuzz/corpus/macro_wasm_lex/suffix-256.hex`.
What: store a longer suffix (a `u16`/`u32` length, or the split kept in the symbol), or give
the callers a fallible constructor.

> forwarded to format: `crates/tt/` is lane format's.
