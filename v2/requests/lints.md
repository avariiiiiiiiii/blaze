# Requests of lane `lints` (crates/blaze-lints)

Design: docs/v2/plan/lanes/lints.md, docs/v2/plan/lanes/lints/cx.md.

## db: `hir::BodyAnalysis`, one body's types and resolutions by pointer into its relative tree

- What: a public type in `crates/hir` (new file `crates/hir/src/body_analysis.rs`, re-exported
  `hir::BodyAnalysis`), built on `SourceAnalyzer`'s internals over
  `hir_def::expr_store::Body::relative`'s source map rather than `body_with_source_map`'s:

  ```rust
  pub struct BodyAnalysis<'db> { /* db, def, file, body, source map; inference in a OnceCell */ }
  impl<'db> BodyAnalysis<'db> {
      /// Reads `body_syntax` and `Body::relative` (for a body a macro writes, the expansion's
      /// tree and `Body::with_source_map`); never `infer`.
      pub fn new(db: &'db Db, def: DefWithBody) -> BodyAnalysis<'db>;
      /// The tree every pointer below is into, and its file: `BodySyntax::root` and its
      /// `file_id`, or the expansion's root and its macro file.
      pub fn tree(&self) -> (SyntaxNode, HirFileId);
      pub fn def(&self) -> DefWithBody;
      pub fn body(&self) -> &'db hir_def::expr_store::Body;
      pub fn source_map(&self) -> &'db hir_def::expr_store::BodySourceMap;
      /// Infers the body the first time it is called.
      pub fn infer(&self) -> &'db hir_ty::InferenceResult<'db>;
      // Each of these infers the body the first time one is asked:
      pub fn type_of_expr(&self, expr: InFile<AstPtr<ast::Expr>>) -> Option<TypeInfo<'db>>;
      pub fn type_of_pat(&self, pat: InFile<AstPtr<ast::Pat>>) -> Option<TypeInfo<'db>>;
      pub fn expr_adjustments(&self, expr: InFile<AstPtr<ast::Expr>>) -> Option<Vec<Adjustment<'db>>>;
      pub fn resolve_method_call(&self, call: InFile<AstPtr<ast::MethodCallExpr>>) -> Option<Function>;
      pub fn resolve_path(&self, path: InFile<AstPtr<ast::Path>>) -> Option<PathResolution<'db>>;
      pub fn binding(&self, pat: InFile<AstPtr<ast::IdentPat>>) -> Option<Local<'db>>;
      /// The callee of a call or method call: the impl's function when the arguments say
      /// which (`lookup_impl_method`), else the trait's.
      pub fn callee(&self, expr: hir_def::hir::ExprId) -> Option<(hir_def::FunctionId, hir_ty::next_solver::GenericArgs<'db>)>;
      /// `ty` with the body's environment, for `Type`'s methods (`is_copy`, `impls_trait`).
      pub fn ty(&self, ty: hir_ty::next_solver::Ty<'db>) -> Type<'db>;
      // No inference:
      pub fn expansion(&self, call: InFile<AstPtr<ast::MacroCall>>) -> Option<MacroCallId>;
      /// Where `expr` is written in `tree()`: its own range, or the outermost macro call of
      /// the tree that wrote it.
      pub fn expr_range(&self, expr: hir_def::hir::ExprId) -> Option<TextRange>;
  }
  ```
- Where: `crates/hir/src/body_analysis.rs` (new), `crates/hir/src/lib.rs` (`mod` and `pub use`).
- Why: blaze-lints runs each lint per body as a query over `body_syntax`'s tree (lints.md), so
  an edit lints only the bodies it changed. `Semantics` answers only for nodes of the file's
  whole tree, through the file-positioned source map, so a query asking it would depend on
  the whole file and run again on every edit above the body. Pointers (`AstPtr`) are the key
  because the lints walk `syntax::view`s and `hir` still takes handles: a pointer is the same
  for both. Lazy inference keeps "a lint that needs only syntax never forces inference".
  Until it exists, blaze-lints is written against this exact API.

> forwarded to db (`crates/hir`; told by tmux).

## diagnostics: register blaze's new lints and the group `blaze::hot`

- What: in `crates/blaze-diagnostics/src/lints/registry/lints.rs`, next to
  `blaze::const_impure`, four lints, each `Allow` by default, and a group:
  - `blaze::hot_clone`: "detects a `.clone()` that allocates or bumps a reference count in hot code";
  - `blaze::hot_alloc`: "detects a call that reaches the global allocator in hot code";
  - `blaze::hot_dyn`: "detects a call through `dyn` in hot code";
  - `blaze::std_hashmap`: "detects std's `HashMap` and `HashSet` with their default hasher";
  - group `blaze::hot` = `hot_clone`, `hot_alloc`, `hot_dyn`.
  The command line accepts `--warn=blaze::hot_alloc` and the rest (from `[lints.blaze]`).
- Why: they run in blaze-lints (`blaze clippy`), but rustc's level machinery in
  blaze-diagnostics sees `#[deny(blaze::hot)]` and `--deny=blaze::hot_alloc`: without them
  registered, it says `unknown_lints`. Allow by default, so nothing changes for a crate that
  doesn't name them (superset-of-rust).

> forwarded to diagnostics (told by tmux).

## codegen: `blaze-engine` hands blaze-lints `&base_db::Db`

- What: `crates/blaze-engine/src/check.rs` `Checker::lints`: call
  `blaze_lints::serve::answer(db, &scope.linted, &self.root, &path_of, &ask)` with `db: &Db`
  (the engine's database once it is `base_db::Db`), `scope.linted: &[base_db::Crate]`,
  `path_of: &dyn Fn(base_db::FileId) -> Option<PathBuf>`. The signature of `answer` is
  otherwise unchanged.
- Why: blaze-lints no longer takes `ide_db::RootDatabase` (deleted at integration).

> forwarded to codegen (told by tmux).

## cargo: `[lints.blaze]` read as `[lints.clippy]` is

- What: wherever blaze-cargo reads a package's `[lints]` for rustc's command line (the check
  and the build, not only `cargo clippy`), a `blaze` tool table becomes
  `--<level>=blaze::<name>` flags as `clippy`'s does, and no "unrecognized lint tool" warning
  is given for it. `blaze_lints::manifest::lints` and `flags` already read it for
  `blaze clippy` (`crates/blaze-cargo/src/cargo/own/clippy.rs` needs no change).
- Why: `[lints.blaze]` is how a crate opts in to blaze's own lints (lints.md), a blaze
  extension: a crate without the table sees nothing new.

> forwarded to cargo (told by tmux).

## coordinator: at integration

- Delete `tools/v2/hot_lint.py` once blaze-lints' `blaze::` lints build and their tests (its
  patterns as fixtures, `crates/blaze-lints/src/blaze/tests.rs`) pass, and add to the root
  `Cargo.toml` `[workspace.lints.blaze]` with `std_hashmap = "deny"`; in each crate of
  hot_lint.py's `HOT` list, `[lints.blaze] hot = "deny"` (its `[lints]` then stops being only
  `workspace = true`: cargo's `[lints]` can't extend the workspace's, so the hot crates spell
  out the workspace table and add `blaze`). Point common.md's hot_lint step at
  `blaze clippy`.
- `crates/blaze-lints/Cargo.toml` drops `ide-db` and gains `base-db`, `hir-def`, `hir-ty`,
  `salsa`, `triomphe` (all workspace dependencies already): no new workspace dependency.

> forwarded to the coordinator, for integration: deleting `tools/v2/hot_lint.py`, the hot crates' `[lints.blaze]`, common.md and blaze-lints' manifest are its. requests adds the root `[workspace.lints.blaze] std_hashmap = "deny"` only then: before blaze-lints' lints build, cargo would warn about the unknown tool.

## frontend-tree: answers on the views (2026-10-29)

1. **Untyped view.** `syntax::view::Node<'t>` is `syntax_tree::view::Node<'t, RustLanguage>`
   (`crates/syntax/src/view.rs` L37). Likewise `syntax::view::{Token, Element, NodeId}` and the
   iterator aliases (`Children`, `ChildrenWithTokens`, `Descendants`, `DescendantsWithTokens`,
   `Preorder`, `PreorderWithTokens`). `Element<'t>` is `NodeOrToken<Node<'t>, Token<'t>>`,
   `NodeOrToken` and `WalkEvent` as today. A view is 24 bytes, `Copy`, `Send + Sync`, and
   compares by `(file, part, element)`. `text()` is `&'t str`. A lazy body is entered as if it
   were in the tree.
2. **Extension layer.** Same names, ported by child `frontend-tree-ast` (running now), into
   `crates/syntax/src/view/{node_ext,expr_ext,token_ext,prec}.rs`.
   - Already there:
     - every generated accessor, for example `MacroCall::token_tree()`;
     - every trait, in `view/traits.rs`: `HasAttrs::attrs`, `attrs_with_doc`,
       `inner_attributes_node`, `HasName`, `HasVisibility`, `HasGenericParams` and the rest,
       plus `attrs_including_inner(&owner)`.
   - Being written: `simple_name`, `String::value`, `BinExpr::op_kind` and the rest of
     `node_ext`, `expr_ext` and `token_ext`.
   - Differences from the handle versions:
     - Methods take `self`.
     - Text that the handle returned owned only because it could not lend it is `&'t str`.
       Expect `simple_name() -> Option<&'t str>` where the handle gives a `SmolStr`.
     - `value()` keeps `Result<Cow<'t, str>, EscapeError>`.
     - `op_kind()` returns the same operator enums, `syntax::ast::{BinaryOp, ArithOp, ...}`,
       reused rather than copied.

   I'll tmux `lints` when the child reports DONE, with any name that differs.
3. **Source maps to a view.** Source maps keep handing out `InFile<AstPtr<N>>`, with `N` the
   handle type used only as a kind tag; no handle is ever made from it. To get a view, write
   `ptr.value.to_view(root)`, which gives `<N as Typed>::Of<'db>`, e.g. `syntax::view::Fn<'db>`.
   `root` comes from the database:
   - For a real file, `editioned_file_id.parse(db).root()` works today. `parse` is
     `returns(ref)`, so the view borrows `&'db Db` and lives as long as it.
   - For any `HirFileId`, use `file_id.root(db: &Db)`. It is requested of db in
     `v2/requests/db.md`, together with `AstId::to_view(db)`, so it is not there until db
     answers.

   `syntax::view::{ptr, to_node, try_to_node}` cover the untyped case (`SyntaxNodePtr`).
   `CfgExpr::parse_from_view(view::CfgPredicate<'_>)` exists in `cfg` beside `parse_from_ast`.

`syntax::view` becomes reachable once frontend-deps adds `pub mod view;` to `syntax/src/lib.rs`,
which is requested in `v2/requests/frontend-deps.md`.

> forwarded to lints (answers for it; told by tmux).

## frontend-db: the API to write blaze-lints against (answer; 2026-10-29)

1. `hir::Semantics` stays and becomes non-generic: `Semantics<'db>` with `pub db: &'db Db`
   (today `Semantics<'db, DB: ?Sized>` holding `&'db DB`, over a `SemanticsImpl` holding
   `&'db dyn HirDatabase`). With one database type, the parameter carries nothing.
   `Semantics::new(db: &'db Db)`. frontend-db's child `frontend-db-hir` makes the change.
2. Queries keep the form they have today; only the database type changes:
   - The free/associated query functions stay as they are, over `&Db`:
     `InferenceResult::of(db, def)`, `Body::of(db, def)`, `Body::with_source_map(db, def)`,
     `hir_def::lang_item::lang_items(db, krate)`, `AttrFlags::lang_item(db, owner)`.
   - What is a `HirDatabase` method today stays one (`db.ty(def)`, `db.mir_body(def)`,
     `db.const_eval(..)`, ...). It is a sealed extension trait with the one impl for `Db`, so
     you need `use hir::db::HirDatabase;` in scope, and the calls are static.
   - `SourceDatabase`'s methods (`db.file_text(..)`, `db.source_root(..)`, ...) are `Db`'s
     inherent methods, with no import.
   Through `hir`'s own API (`hir::Function`, `hir::Module`, ...), every method that took
   `db: &dyn HirDatabase` takes `db: &Db`.
3. `base_db::EditionedFileId` (the interned one, `crates/base-db/src/editioned_file_id.rs`) is
   what queries take; `span::EditionedFileId` is its raw value and `base_db::FileId` the VFS
   id (`vfs::FileId`, re-exported). Keep using them as `hir` re-exports them today.

Test databases: `type TestDB = base_db::Db;` made by `Db::for_tests()` (no `Default`), with
`use hir as _;` so the MIR providers are linked. `test-fixture`'s `WithFixture` is implemented
for `Db`.

> forwarded to lints: an answer for its asker, nothing to apply.

> applied by frontend-db-hir (item 1): `hir::Semantics<'db>` is non-generic, `pub db: &'db Db`, made by `Semantics::new(db: &'db Db)` or `new_within`; `new_dyn` is gone. `blaze-lints/src/check.rs`'s `Semantics<'db, RootDatabase>` became `Semantics<'db>`.
