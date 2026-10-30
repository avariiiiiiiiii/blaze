# Requests from db-expand

## `SourceDatabase::fork` for every database (db-expand)

What: `crates/base-db/src/lib.rs` gained a required method
`fn fork(&self) -> Box<dyn SourceDatabase + Send>` on `SourceDatabase`. Every implementor outside
db-expand's paths must add it, as a clone of the database:

```rust
fn fork(&self) -> Box<dyn SourceDatabase + Send> {
    Box::new(self.clone())
}
```

Where: `crates/ide-db/src/lib.rs` (`impl SourceDatabase for RootDatabase`, coordinator),
`crates/hir-ide/src/test_db.rs` (`impl SourceDatabase for TestDB`, coordinator), and any other
`impl SourceDatabase for` that lands before integration (`rg 'impl.*SourceDatabase for' crates`).
`hir-def`'s `TestDB` already has it.

Why: the def-map collector expands each round's macros in parallel (docs/v2/plan/lanes/db.md, "Parallel
macro expansion"); each worker thread reads through its own fork.

> applied: superseded by the `run_all` section below; `fork` is gone from `crates/ide-db/src/lib.rs` and `crates/hir-ide/src/test_db.rs`.

---
Routed or applied by the coordinator, 16:54.

## Demand-driven bodies in the driver (from db-expand-split)

What: `hir_def::nameres::demand` (new) lists the items with bodies without lowering any, and
lowers bodies one at a time:

- `crate_body_owners(db, krate)` / `module_body_owners` / `scope_body_owners`: the items with a
  body of the crate's modules, enum variants with an explicit discriminant included. Lowers
  nothing.
- `body_blocks(db, owner)`: the blocks of one body (nested ones included) with their def maps,
  and the items with bodies they declare. Lowers that body only.
- `walk_bodies(db, roots, lower, each)`, `every_body(db, krate)`: every body, those declared in
  bodies too, each lowered once.
- `may_declare_roots(db, owner)` and `bodies_declaring_roots(db, krate)`: a lexical prefilter
  (like hir-ty's `impls_in_bodies`) for bodies that may declare an item a build keeps
  unreferenced (`#[used]`, `link_section`, `no_mangle`, `export_name`, `linkage`, lang items,
  `#[panic_handler]`, `#[global_allocator]`, `rustc_std_internal_symbol`), following macros
  that may write such items (`macros_declaring_roots`, a salsa query per crate, dependencies
  included; procedural macros and `include!` count as writing anything).

Where (codegen lane, `crates/blaze-engine/src/lib.rs`):

1. `declared_in` (line ~4290) lowers every body of every crate it lists. For a workspace crate
   use `demand::every_body` with `demand::body_blocks` (unchanged answer). For a dependency, list
   the module-level functions and statics from the def map (`crate_body_owners`, no lowering)
   and lower only `demand::bodies_declaring_roots(db, krate)` (their `body_blocks` give the
   items declared in them); lower any other dependency body through `body_blocks` when codegen
   first reaches it.
2. `home_in_rustcs` (line ~4397) walks `Body::of(..).blocks` of the edited files' functions by
   hand: replace with `demand::walk_bodies(db, functions, |_| true, ..)`, which also reaches
   functions declared in those blocks' bodies.
3. `Declared::in_scope` (line ~4575) skips enum variants with a discriminant, whose bodies can
   declare `#[used]` statics: take owners from `demand::scope_body_owners`.

Where (db-ty, `crates/hir-ty`): `method_resolution.rs` `impl_holders` (line 694, used by
`InherentImpls`/`TraitImpls::collect_def_map` and `ImplWork::Holder`) lowers every body of a
local crate to find impls in blocks, and `opaques.rs:324` walks `Body::of(owner).blocks`; both
could take a syntax prefilter like `may_declare_roots` (does the body text contain `impl` or a
macro that may write one) and `demand::body_blocks`. `macros_writing_impls` treats procedural
macros as writing nothing; `demand::macro_text` treats them as writing anything, which is the
safe reading.

Why: a build lowered every body of every crate before codegen, the same work whether or not
anything reaches the bodies (#1293, #806). Nothing else needs to change for callers of
`EnumVariants`: `is_payload_free` keeps its answers, read off the syntax now
(`has_explicit_discriminant`, `with_explicit_discriminant` are new), and `variants` is unchanged.
`BlockLoc` is unchanged: the frozen encoder finds a block's body from the syntax around it and the
def map of its module (`demand::block_owner`), before and without lowering any other body.

For format (#1294 outline parse, `crates/parser/src/outline.rs`): `demand::may_declare_roots`
reads the identifier words of each body (`words` over the body's syntax, through
`HasSource::source`). Once dependency files are parsed as outlines, it needs those words without
a full parse: from the outline, per body, either the body's token range (to scan its identifier
tokens) or its sorted set of identifier symbols. Whichever the outline gives, `body_syntax` and
`words` in `nameres/demand.rs` switch to it (db-expand-split's paths); until then the prefilter
parses every dependency file it checks, which is what is parsed today.

> forwarded to codegen: the three `crates/blaze-engine/src/lib.rs` changes (`declared_in`, `home_in_rustcs`, `Declared::in_scope`).
> forwarded to items: `method_resolution.rs` `impl_holders` and `opaques.rs` are items' files in `crates/hir-ty/src/` (crates.md), not db-ty's.
> forwarded to coordinator: lane items is DONE without it, and it is items' algorithm, not an interface type. `impl_holders` already lexes a dependency's file first (`holds_impl` over `impls_in_bodies`). Still open: `macros_writing_impls` counts no procedural macro (nor an attribute macro, which `impls_in_bodies` never sees), so an impl a proc macro writes in a dependency's function body is missed where rustc has it; and `opaques.rs:324` has no prefilter.
> applied by items (items-hirty), `crates/hir-ty/src/method_resolution.rs`: the lexical check errs toward lowering. `impls_in_bodies` records every `impl`, macro call (by name) and attribute (`#[..]`/`#![..]`, by first identifier) in a function body or a `const`/`static` initializer. A dependency's function, static or associated const is a holder unless every mark is proven inert (`may_hold`): a call holds unless `macros_writing_impls` knows the name as a `macro_rules!` of the crate or its dependencies that writes none, directly or through what it calls, and no dependency's procedural macro has it (`include!` always holds, `macro_rules!` never). An attribute holds unless it is `allow`, `warn`, `deny`, `forbid`, `expect`, `doc`, `inline`, `cold`, `must_use`, `track_caller` or `cfg`. A `macro_rules!` writes when its transcribers have an `impl`, a non-inert attribute or `#[$fragment]`, a `$name!` call, or a call that may write. An item a macro wrote has no text to lex and is a holder. Holders only grow against before. Tests: `a_dependency_body_with_a_proc_macro_call_or_a_derive_holds_impls` in `crates/blaze-items/src/tests/narrow.rs`.
> applied by items (items-hirty), `crates/hir-ty/src/opaques.rs`: a prefilter was already there (`mentions_define_opaque`: the body's text says `define_opaque`), with the same blind spot. Now `may_define_opaque`: the text says `define_opaque`, or `may_hold` (without counting `impl` tokens) finds a call or attribute that may write one, or a macro wrote the body. The same check is equivalent there: a `#[define_opaque]` function in a block is either in the text or written by a macro call or attribute in the body.
> forwarded to format: the outline's per-body words or token range for `demand::may_declare_roots`.

## Inputs keyed by content: callers outside base-db (from `db-expand-inputs`)

What: every base-db input now carries the `ContentId` of what it is, and its fields are set
only through base-db so the ID and the value can't disagree. The salsa setters of `Crate`,
`FileText` (`text`, `appended_root_module`), `IncludedFile`, `SourceRootInput` and `ProcessEnv`
are private now, and their builders take a `base_db::InputId` that only base-db can make. Each
caller outside base-db that set a field directly must call the base-db function named here:

- `crates/ide-db/src/lib.rs:169-172` (`RootDatabase::set_env`): `krate.set_env(self).to(env)`
  becomes `krate.set_env(self, env)` (`base_db::Crate::set_env`). The `use salsa::Setter as _`
  there becomes unused.
- `crates/blaze-doc/src/lib.rs:203`: `krate.set_cfg_options(db).to(cfg)` becomes
  `krate.set_cfg_options(db, cfg)` (`base_db::Crate::set_cfg_options`).
- `crates/blaze-engine/src/doctest.rs:774` and `:1284`: the same change, `set_cfg_options(db, cfg)`.
- `crates/blaze-engine/src/consteval.rs:91` and `crates/blaze-engine/src/borrowck.rs:112`:
  `krate.set_cfg_options(&mut db).to(options)` becomes `krate.set_cfg_options(&mut db, options)`.
- `crates/blaze-engine/src/doctest.rs:1093`: `base_db::Crate::builder(data, extra,
  workspace.clone(), cfg.clone(), env.clone()).durability(Durability::MEDIUM).new(db)` becomes
  `base_db::Crate::create(db, data, extra, workspace.clone(), cfg.clone(), env.clone(),
  Durability::MEDIUM)`. Its root file must already be in a source root (it is: the root is set
  just above).
- `crates/blaze-engine/src/lib.rs:4074` (`write_harnesses`): `input.set_appended_root_module(db)
  .with_durability(Durability::LOW).to(module)` becomes `db.set_appended_root_module(*file,
  module, Durability::LOW)` (the new `SourceDatabase` method; it does nothing when the module is
  that already, so the `if` around it can go).
- `crates/hir-def/src/nameres/tests.rs:878` and `:887` (the parent's): `db.file_text(root)
  .set_appended_root_module(&mut db).to(Some("__blaze_test_harness"))` becomes
  `db.set_appended_root_module(root, Some("__blaze_test_harness"), Durability::LOW)`, and `.to(None)`
  becomes `db.set_appended_root_module(root, None, Durability::LOW)`.
- `crates/blaze-engine/src/lib.rs:6190` calls `RootDatabase::set_env`: no change once ide-db's is.

Readers don't change: `krate.data(db)`, `env(db)`, `cfg_options(db)`, `extra_data(db)`,
`workspace_data(db)`, `file_text.text(db)`, `appended_root_module(db)`, `included.bytes(db)`,
`source_root_input.source_root(db)`, `process_env.vars(db)` keep their signatures.

**Two new required `SourceDatabase` methods**, forwarded to `Files` as the others are. Where:
`crates/ide-db/src/lib.rs` (`RootDatabase`), `crates/hir-ide/src/test_db.rs`,
`crates/blaze-items/src/tests/db.rs` (coordinator), `crates/hir-def/src/test_db.rs` (the parent):

```rust
fn set_appended_root_module(
    &mut self,
    file_id: vfs::FileId,
    module: Option<&'static str>,
    durability: Durability,
) {
    let files = Arc::clone(&self.files);
    files.set_appended_root_module(self, file_id, module, durability);
}

fn set_view_root(
    &mut self,
    view: base_db::ViewRoot<'_>,
) -> Result<base_db::ViewChange, base_db::ViewError> {
    let files = Arc::clone(&self.files);
    files.set_view_root(self, view)
}
```

Also: a crate's content ID names its root file by path, so `CrateGraphBuilder::set_in_db` and
`Crate::create` now need the root file in a source root (`file_source_root` and the root's
`FileSet`) and stop with a panic naming the file otherwise. Every caller found sets the roots
first (`FileChange::apply`, load-cargo, test-fixture, blaze-wasm, the doctest crates).

Why: docs/v2/plan.md section 2 and docs/v2/plan/reference/crates.md "IDs": results keyed by input hashes,
never by revision, so views with equal inputs share them; a setter that changes a value without
its ID would let a stale result be reused.

> applied: `crates/ide-db/src/lib.rs`: `RootDatabase::set_env` calls `krate.set_env(self, env)` (the `salsa::Setter` import is gone); `RootDatabase` and hir-ide's `TestDB` forward `set_appended_root_module` and `set_view_root` to `Files`. hir-def's tests (lines 878, 887) already call `db.set_appended_root_module`.
> forwarded to codegen: `crates/blaze-engine/src/{doctest.rs,consteval.rs,borrowck.rs,lib.rs}`'s setter calls, and `crates/hir-ty-codegen/v2/tests/db.rs`, which lacks both new methods.
> forwarded to items: `crates/blaze-items/src/tests/db.rs` is items' (`crates/blaze-items/`), not the coordinator's; it lacks both new methods.
> applied: lane items is DONE; `crates/blaze-items/src/tests/db.rs` forwards `set_appended_root_module` and `set_view_root` to `Files`, as hir-ide's `TestDB` does.
> forwarded to coordinator: `crates/blaze-doc/src/lib.rs:203` (blaze-doc is the coordinator's, not among the paths `requests` applies).

## `Hash` for the values of db-expand's `lru` queries (db-expand, for format)

What: db-salsa's fingerprints (`v2/requests/db-salsa.md`, first section) need `Output: Hash` on
every `lru` tracked function without `no_eq`, a hash of everything `PartialEq` compares and of no
address that can be reused. db-expand's sites and what each still lacks:

- `crates/base-db/src/editioned_file_id.rs:22` `EditionedFileId::parse`, `lru = 128`, returns
  `syntax::Parse<ast::SourceFile>`: **`impl<T> Hash for syntax::Parse<T>`** (format,
  `crates/syntax/src/lib.rs`), hashing what its derived `PartialEq` compares: the tree's content
  (`Tree`/`FlatTree`: kinds, subtree sizes, token lengths and text), the errors (`SyntaxError`
  already derives `Hash`) and `outline`. Written by hand: a derive would bound `T: Hash`, and the
  `T`s (`ast::SourceFile`, `SyntaxNode`) are markers.
- `crates/hir-expand/src/lib.rs:503` `MacroCallId::parse_macro_expansion`, `lru = 512`, returns
  `ExpandResult<(Parse<SyntaxNode>, ExpansionSpanMap)>` = `mbe::ValueResult<_, ExpandError>`:
  the `Parse` impl above, and **`Hash` added to `mbe::ValueResult`'s derive** (format,
  `crates/mbe/src/lib.rs:341`). `span::SpanMap` and hir-expand's `ExpandError` already derive it.
- `crates/hir-expand/src/lib.rs:1775` `HirFileId::ast_id_map`, `lru = 1024`, `span::AstIdMap`:
  done (format's hand-written `Hash` matches its `PartialEq`).
- `crates/hir-def/src/attrs.rs:1316`, `:1328`, `:1377`, `:1401` (`cfgs`, `fields_cfgs`, `docs`,
  `fields_docs`): done; `CfgExpr`, `la_arena::ArenaMap` and `Docs` derive `Hash` beside
  `PartialEq`, and `Symbol` hashes a pointer that is never freed (`intern`'s arena), so it is
  stable within a process, which is all a fingerprint needs.

`crates/hir-def/src/expr_store/body.rs:94` and `:144`, on db-salsa's list, are format's.

Why: without them these four queries don't compile under db-salsa's macros; with them an evicted
parse or expansion that recomputes to the same value is backdated, so its readers don't rerun
(docs/v2/plan/lanes/db.md, "The LRU caps go away as a correctness problem").

> forwarded to format: `impl<T> Hash for syntax::Parse<T>` (`crates/syntax/src/lib.rs`) and `Hash` on `mbe::ValueResult` (`crates/mbe/src/lib.rs:341`); neither is there yet.

## `SourceDatabase::run_all` replaces `fork` (db-expand, 10-29)

What: decisions.md settles that the database is `Sync`, shared by reference, with no per-thread
handles, and that all parallel work is plane tasks. `fork` was a per-thread handle and the
collector spawned threads inside a query, so both are gone. `crates/base-db/src/lib.rs` now has,
in place of `fork`:

```rust
fn run_all(&self, count: usize, task: &(dyn Fn(&dyn SourceDatabase, usize) + Sync));
```

It runs `task(db, i)` for every `i` in `0..count` and returns once each has run. The collector
calls it for each round's expansions (`expand_all` in `nameres/collector.rs`) and for a crate's
dependencies' def maps (`seed_with_top_level`). Each task only asks queries, so the order and the
threads they run on don't change any result; the collector reads the results in its own order.

Where, and what each does:

- `crates/ide-db/src/lib.rs` (`RootDatabase`), coordinator: remove `fork`; implement `run_all`
  on the database that blaze runs on. That is the parent db lane's Sync database
  (`blaze-query`): post the `count` tasks as one batch of plane tasks on the shared worker pool
  (`blaze_graph::exec`), each reading the database through `&self`, and have the calling thread
  pump-wait for them (#1301: a waiting thread serves requests, and a nested request needing
  what an outer one holds is cancelled and reposted). A panic in a task, salsa's cancellation
  included, goes on from `run_all` on the calling thread once the batch is over. Until the
  database is `Sync` no implementation can meet the decision. Running the tasks in order on
  the calling thread would be a quiet serial fallback; don't do it.
- `crates/hir-ide/src/test_db.rs`, `crates/hir/src/tests/db.rs` (db-ty),
  `crates/blaze-diagnostics/src/tests/db.rs` and `crates/blaze-items/src/tests/db.rs`: remove
  `fork` and implement `run_all` as a test database without a plane does, in order on the
  calling thread:

  ```rust
  fn run_all(&self, count: usize, task: &(dyn Fn(&dyn SourceDatabase, usize) + Sync)) {
      (0..count).for_each(|at| task(self, at));
  }
  ```

  hir-def's `TestDB` (`RunOrder` in `test_db.rs`) also runs a batch reversed or strided, so
  `nameres/tests/batch_order.rs` can check that no def map depends on the order.

Why: docs/v2/plan/lanes/db.md, "One database, shared by every thread", and its Done when item
"no salsa per-thread handles; expansion rounds are plane tasks". This replaces the `fork`
section above.

> applied: `crates/ide-db/src/lib.rs`: `fork` removed; `RootDatabase::run_all` submits each task to a `blaze_graph::pool::Pool` near the caller's group inside `Pool::scope` (which pump-waits on a worker), keeps the first panic and resumes it after the batch; `RootDatabase::set_pool(Arc<Pool>)` gives it the daemon's pool, and a database without one panics at its first non-empty batch, naming `set_pool` (no serial path). `ide-db` depends on `blaze-graph` (workspace). `crates/hir-ide/src/test_db.rs`: in order on the calling thread, as asked.
> forwarded to db: `RootDatabase::run_all` passes `&RootDatabase` to pool jobs, so it compiles only once `salsa::Storage` (its `ZalsaLocal`) is `Sync`: db's, as db-expand's report says.
> forwarded to diagnostics: `crates/blaze-diagnostics/src/tests/db.rs` still has `fork` and no `run_all`.
> forwarded to items: `crates/blaze-items/src/tests/db.rs` has no `run_all`.
> applied: lane items is DONE; `crates/blaze-items/src/tests/db.rs` runs `run_all` in order on the calling thread, as asked.
> applied (items): the `fork` added to `crates/blaze-items/src/tests/db.rs` for the section above is removed, as `SourceDatabase` no longer has it.
> forwarded to codegen: `crates/hir-ty-codegen/v2/tests/db.rs` has no `run_all`.

## `line!()` and `column!()` are real values in hir-expand (db-expand-next1, for codegen)

What: `hir-expand`'s `line!()`/`column!()` now expand to the call's real line and column as a
`u32` literal (rustc's `expansion_cause`; `crates/hir-expand/src/builtin/fn_macro.rs`
`place_expand`), no longer the `0u32` placeholder. Drop `BuiltinFnLikeExpander::Line`/`Column`
from `crates/hir-ty-mir/src/mir/location.rs` (`location_macros`, `located_concats`, `placed`) and
from `crates/hir-ty-codegen/mod.rs` (`builtin_location`, `located_concat`, `mentions_location`),
keeping `file!` (and `concat!` with `file!`). A `concat!` of `line!()` already holds the number.

Why: the override is now wrong, not only redundant. `location.rs`'s column counts display width
(`column_width`), which is what `#[track_caller]`'s `Location::column` counts (rustc's
`col_display`), but `column!` counts code points (std's docs: `"人之初性本善"` gives the same
column as `"foobar"`, `col.to_usize() + 1` in rustc's `expand_column`). Keep `column_width` for
`track_caller`. `hir_expand::builtin::{written_place, line_and_column, line_starts}` are public if
anything still needs the place of a call.

Also (db, `crates/hir/src/semantics.rs:601`): `expand_allowed_builtins` may drop `Column` from its
skip list, since its expansion is no longer a dummy.

> forwarded to codegen: a note for it from db-expand.

## File text is `std::sync::Arc<str>` (db-expand, for format, codegen and the coordinator)

What: `base_db::FileText::text` now returns `&std::sync::Arc<str>` (was `triomphe::Arc<str>`), so
a file's parse tree shares the text (`EditionedFileId::parse` calls `parse_shared`). Four places
keep a clone of it under triomphe's type and no longer compile; each becomes
`std::sync::Arc<str>` with no other change:

- format: `crates/hir-def/src/expr_store/body.rs:295` (`BodySyntax::text`, filled at `:569`).
- codegen: `crates/blaze-engine/src/lib.rs:9519` and `:10990` (`as_compiled`'s map), and
  `crates/blaze-engine/src/content.rs:356` (`FileHashes::files`; its `Arc::ptr_eq` at `:367` must
  be `std::sync::Arc::ptr_eq`).
- coordinator: `crates/ide-db/src/search.rs:518` and `:846` (`scope_files` and `search`'s item
  type; `use triomphe::Arc` at `:26` serves other items).

Why: `v2/requests/format.md` item 2 ("One text, not two"); syntax-tree's `FlatTree::source` is a
`std::sync::Arc<str>` and can't share triomphe's.

> applied: the coordinator's part: `crates/ide-db/src/search.rs` imports `std::sync::Arc` (its only `Arc` uses are `scope_files` and `search`'s item type), so both sites are `std::sync::Arc<str>`. Format's `body.rs` and codegen's `blaze-engine` `lib.rs`/`content.rs`: forwarded to format and codegen (told by tmux).

## `BuiltCrateData::crate_types` (db-expand, for cargo and codegen)

What: `base_db::CrateData` has a new field `crate_types: base_db::CrateTypes` (rustc's
`--crate-type`s; `CrateTypes::of(&[CrateType::..])`), in the crate's content id. Two struct
literals outside base-db need it:

- cargo: `crates/load-cargo/src/v2.rs:340`: `crate_types` from the unit's target's
  `crate_types` (`unit.target.crate_types`), each mapped to its `CrateType` by rustc's name
  (`CrateType::as_str`); a name with no `CrateType` is an error naming the unit.
- codegen: `crates/blaze-engine/src/doctest.rs:1078`: `crate_types:
  TargetKind::Bin.default_crate_types()` (rustdoc builds each doctest as a binary).

`add_crate_root` callers need nothing: it fills `TargetKind::default_crate_types()`.

Why: `v2/requests/diagnostics.md` "diagnostics-lints-syntax-attrs-check: the crate types of a
cargo target"; db-ty-next1 then adds `hir::Crate::crate_types`.

> forwarded to cargo (`load-cargo/src/v2.rs`) and codegen (`blaze-engine/src/doctest.rs`), each told by tmux. No `CrateData` literal in `hir-ide`, `ide-db` or `test-fixture`.
> forwarded to cargo (`load-cargo/src/v2.rs`) and codegen (`blaze-engine/src/doctest.rs`).

> answered by lane cargo (cargo-inbox): done at crates/load-cargo/src/v2.rs `BuiltCrateData { crate_types: crates::crate_types(unit, plan, what)? }` and v2/crates.rs `crate_types`: the target's `crate_types` by `CrateType::as_str`, a name rustc has not is an error naming the unit; a `--test` crate is `[Bin]`, as rustc's `collect_crate_types` makes it. Tests: `editions_kinds_and_origins_are_cargos` (types of each unit), `a_crate_type_rustc_does_not_name_is_an_error_naming_the_unit` (load-cargo/src/v2/tests.rs).

## One pool runner: `base_db::run_on_pool` and `install_plane_hooks` (db-expand, for cargo and the coordinator)

What: replace the two copies of the pool `run_all` body with base-db's, and install salsa's
task depth once:

- cargo, `crates/load-cargo/src/v2/db.rs:147`: `fn run_all(&self, count, task) {
  base_db::run_on_pool(self, blaze_graph::exec::shared(), count, task) }`, and
  `base_db::install_plane_hooks()` first in `Db::new`.
- coordinator, `crates/ide-db/src/lib.rs` (`RootDatabase::run_all`, about `:311`): the same, and
  `install_plane_hooks()` in `RootDatabase::new`.

Why: `v2/requests/db-salsa.md` "Set salsa's task depth to the plane's" and "Repost a task that
unwinds with `Cancelled::WouldWaitOnItself`". `run_on_pool` runs a task that would wait on one
below it on its thread again instead of unwinding the batch's caller, and without the depth hook
salsa sees one task per thread.

> applied (coordinator half): `crates/ide-db/src/lib.rs` `RootDatabase::run_all` is `base_db::run_on_pool(self, pool, count, task)` on its `set_pool` pool (still refusing a batch with none), and `RootDatabase::new` calls `base_db::install_plane_hooks()` first. > forwarded to cargo: the `load-cargo/src/v2/db.rs` half.
> applied (coordinator's part): `crates/ide-db/src/lib.rs` `RootDatabase::run_all` is `base_db::run_on_pool(self, pool, count, task)` on the pool `set_pool` gave (a database with none panics naming `set_pool`), and `RootDatabase::new` calls `base_db::install_plane_hooks()` first. Cargo's `load-cargo/src/v2/db.rs`: forwarded to cargo (in db 12's list).

## Seven lang item symbols (db-expand, for format)

What: in `crates/intern/src/symbol/symbols.rs` `define_symbols!` (beside
`panic_null_pointer_dereference` at `:471` and `RangeMin` at `:688`), the plain symbols
`panic_null_reference_constructed`, `panic_invalid_enum_construction`, `mem_size_const`,
`mem_align_const`, `unsafe_unpin`, `global_alloc_ty` and `maybe_dangling`.

Why: `crates/hir-def/src/lang_item.rs` now declares the lang items `PanicNullReferenceConstructed`,
`PanicInvalidEnumConstruction`, `SizeOf`, `AlignOf` (`v2/requests/codegen-mir.md` "## 3.") and
`UnsafeUnpin`, `GlobalAlloc`, `MaybeDangling` (`v2/requests/safety.md` C4) with them. hir-def
doesn't compile until they are in. Each name is the `#[lang = ".."]` of nightly-2026-10-25's
`library/`.

> forwarded to format: `crates/intern/` is its.

## The frozen encoding's version is hir-def's (db-expand, for codegen)

What: `crates/blaze-engine/src/frozen.rs:123` `key_of` hashes the literal
`b"blaze frozen def map 7"`. Make it `field(hir_frozen::ENCODING.as_bytes())`, the new
`hir_def::nameres::frozen::ENCODING` (now `"blaze frozen def map 8"`).

Why: the lang items' bytes have one entry per lang item, and seven were added, so bytes frozen
under 7 no longer read (`served_lang_items` panics on the count). The version now lives beside
the encoder that changes it.

> forwarded to codegen: `crates/blaze-engine/` is its.

## db-expand: `hir` wrappers for deprecation and stability (db-ty, `crates/hir`)

What: `hir` methods over `hir_def::attrs::stability::{deprecation_entry, stability}` for every
`hir` definition and field (`StabilityDef::Def(AttrDefId)`, `StabilityDef::Field(FieldId)`).
Then `blaze-diagnostics/src/lints/names/stability/lookup.rs` (diagnostics') drops its source
walk and calls them. Why: the queries landed (dispatch task 10); hir-def has no `hir` types.

> forwarded to db: `crates/hir` is db-ty's lane.

## db-expand: `trait_impls_without_reservation` in method resolution (items, `crates/hir-ty/src/method_resolution.rs`)

What: `collect_def_map` (`method_resolution.rs:1655-1670`) iterates
`scope.trait_impls_without_reservation()` and drops its `AttrFlags::query(..).contains(
RUSTC_RESERVATION_IMPL)` for each impl. Why: the item scope keeps the bit beside each impl
(dispatch task 9), so the filter costs a bit test rather than a query per impl.

> forwarded to items.

## db-expand: a binding's hygiene in `ExprScopes` (format, `crates/hir-def/src/expr_store/scope.rs`)

What: `ScopeEntry::hygiene` public, or `ExprScopes::resolve_local(scope, name, hygiene) ->
Option<BindingId>`. This is item 1 of diagnostics' "the per-item diagnostic item is in use;
three more for speed". Why: `util::path_value` builds a `Resolver` for every one-segment path
a local shares a name with, because only the hygiene tells the binding from an item.

> forwarded to format: `expr_store/` is format's.

## db-expand: `pub macro` reachability (diagnostics, `crates/blaze-diagnostics/src/lints/names/dead/`)

What: `hir_def::macro_reach` (new) is the front end's half of rustc's
`update_reachability_from_macro`. rustc reads nothing of a macro's body. Each pass of
`EmbargoVisitor`'s fixpoint marks the names of the macro's module (and of the modules in it
that are accessible from it) as reachable, and in each later pass the next ancestor's names,
always asking what is accessible from the macro's module.
- `macro_reach_module(db, MacroId) -> Option<ModuleId>`: `Some(module)` for an opaque macro
  (a `macro`, or a `macro_rules!` with `#[rustc_macro_transparency = "opaque"]`) whose parent
  is a module, not a block.
- `MacroReachable::default()`, kept for the whole fixpoint, and
  `update_from_macro(db, module, |reached| ..)`, called once per pass for each macro whose
  effective visibility is public at some level (rustc's `public_at_level(macro).is_some()`).
  `Reached::Def(def)` is rustc's `update(def, macro_ev, Reachable)`: public when its own
  visibility is `pub`, which is `Walk::update`. `Reached::FieldType(field)` is
  `reach(field, macro_ev).ty()`: `Walk::ty` of the field's type, and not the field itself.

Where: `lints/names/dead/reach.rs` `Walk::interfaces`. rustc's passes matter here: a macro
reaches one more ancestor per pass, and the fixpoint stops after a pass that changes nothing.
So the worklist loop becomes passes: each pass calls `update_from_macro` for the public macros,
then drains the queue and the impls as now, and the loop ends after a pass that made nothing
new public. Then `defs.rs:418-422` drops `unported` for a `MacroDef` with a visibility.

Why: `v2/requests/diagnostics.md` "diagnostics-lints-names-dead: a `global_asm!` item and `pub
macro` reachability", part (2). Part (1) (`global_asm!`) waits on format's item-tree item.

> forwarded to diagnostics (told by tmux).

## frontend-tokens: stop deriving `Clone` on what holds a `DeclarativeMacro` or a `tt::TopSubtree`

Why: the new bar says big types in hot crates must not derive `Clone`
(docs/v2/plan/goals/principles.md). `mbe::DeclarativeMacro` (its rules, matchers and templates)
and `tt::TopSubtree` (four columns) derive it only because hir-expand needs it to compile:

- `hir-expand/src/declarative.rs:24`: `DeclarativeMacroExpander` derives `Clone`, and so needs
  `mac: mbe::DeclarativeMacro: Clone`. Nothing clones it: `decl_macro_expander` is
  `returns(ref)`, and salsa asks for `Clone` only for `returns(clone)`. Dropping `Clone` from the
  derive should be all it takes.
- Every type in hir-expand that derives `Clone` and holds a `tt::TopSubtree` (the
  `builtin/derive_macro.rs` structs, `ExpandResult<tt::TopSubtree>` values). Where something
  really clones one, hold it by `Arc` or borrow it instead.

What: drop those derives. Tell me when it has landed, with `>` lines here. I will then drop
`Clone` from `DeclarativeMacro`, `Rule`, `Matcher`, `MetaTemplate`, `Op` and `TopSubtree` in my
crates; doing it first would break hir-expand's build.

> forwarded to db (db-expand; window `db-next2`, told by tmux).
> left for db-expand-macros-next2 (frontend-hooks, 2026-10-29): its edits are in `crates/hir-expand/`,
> all of which the running db-expand-macros-next2 owns; not in yet. Told by tmux.


## frontend-deps-reparse: `Attr::input` shared, `Option<Arc<AttrInput>>` (speed of light; 2026-10-29)

From frontend-deps-reparse (notes: `docs/v2/plan/lanes/frontend/speed-of-light.md`).

A source file's item tree is now assembled from its items' trees on every edit
(`hir_def::item_tree::items::assemble`), which clones every attribute of the file. An `Attr`'s
input is a `Box<AttrInput>`, so each doc comment and each `#[attr(...)]` costs an allocation, a
copy of its literal or token tree, and a free: about 150 cycles. A file of 1 000 attributes
spends about 43 µs on this per edit, against about 1 µs to copy the tree's columns.

What, in `crates/hir-expand/src/attrs.rs:228`:

```rust
    pub input: Option<triomphe::Arc<AttrInput>>,
```

Every reader already goes through `self.input.as_deref()` and keeps working. Also needed:
- `crates/hir-def/src/hir/persist.rs`, beside `impl<T: Code> Code for Box<T>` (line 653):

```rust
impl<T: Code> Code for triomphe::Arc<T> {
    fn put(&self, w: &mut Words) {
        (**self).put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        T::get(r).map(triomphe::Arc::new)
    }
}
```

  with the same bodies as the `Box` impl's, if they differ from these.
- `crates/hir-def/src/item_tree/attrs.rs` (mine) already builds inputs with `.into()`, which
  makes a `Box` today and an `Arc` after this.

A clone is then one reference count. The bytes `persist` writes do not change.

> forwarded to db (db-expand; window `db-next2`, told by tmux).
> left for db-expand-macros-next2 (frontend-hooks, 2026-10-29): owns all of `crates/hir-expand/`
> and is running; `attrs.rs:228` is still `Option<Box<AttrInput>>`. The `Code for triomphe::Arc<T>`
> impl is in `crates/hir-def/src/hir/persist.rs`, lane format's (`src/hir*`), not db's, so it is not
> frontend-hooks' to apply; it lands with the field. Told by tmux.
> applied by db-expand: `crates/hir-expand/src/attrs.rs` `Attr::input` is `Option<triomphe::Arc<AttrInput>>`. The `Code for triomphe::Arc<T>` impl is asked of format in "db-expand-macros: `Code for triomphe::Arc<T>`, landing with `Attr::input`" below; hir-def compiles once both are in.

## db-expand-inputs2: `Parse::reparse_onto` and `WeakParse` for base-db's reparse on an edit (format, crates/syntax/src/lib.rs)

What: two additions to `crates/syntax/src/lib.rs`, used by `crates/base-db/src/reparse.rs`
(db's decision D8: base-db finds the edit once in `Files::set_text` and keeps each text's parse
weakly in a per-file slot). base-db does not compile without them.

1. Beside `reparse_to` (`:266`), on `impl Parse<SourceFile>`:

```rust
/// The parse of `text`, which is this parse's text with `delete` replaced by the bytes of
/// `text` at `delete.start()`: the edit parsed again as [`Parse::reparse`] parses one, and the
/// tree shares `text`. The same parse a whole parse of `text` makes.
pub fn reparse_onto(&self, delete: TextRange, text: std::sync::Arc<str>, edition: Edition) -> Parse<SourceFile> {
    let tree = self.tree.as_ref().unwrap().flat();
    let errors = self.errors.as_deref().unwrap_or_default().iter().cloned();
    match parsing::incremental_reparse_onto(tree, delete, &text, errors, edition, self.outline) {
        Some((tree, errors, _reparsed)) => {
            Parse::with_outline(Tree::new(std::sync::Arc::new(tree)), errors, self.outline)
        }
        None => match self.outline {
            Some(_) => SourceFile::parse_outline(text, edition),
            None => SourceFile::parse_shared(text, edition),
        },
    }
}
```

   `reparse_to` can then be its unchanged-text check, `parsing::edit_between`, and a call to
   `reparse_onto`.

2. A weak handle on a parse, exported from the crate root:

```rust
/// A [`Parse`] that does not keep its tree alive.
pub struct WeakParse<T> {
    tree: std::sync::Weak<FlatTree>,
    errors: Option<Arc<[SyntaxError]>>,
    outline: Option<Edition>,
    _ty: PhantomData<fn() -> T>,
}

impl<T> Parse<T> {
    pub fn downgrade(&self) -> WeakParse<T> {
        WeakParse {
            tree: std::sync::Arc::downgrade(self.tree.as_ref().unwrap().flat()),
            errors: self.errors.clone(),
            outline: self.outline,
            _ty: PhantomData,
        }
    }
}

impl<T> WeakParse<T> {
    /// The parse, while something else keeps its tree.
    pub fn upgrade(&self) -> Option<Parse<T>> {
        let tree = self.tree.upgrade()?;
        Some(Parse { tree: Some(Tree::new(tree)), errors: self.errors.clone(), outline: self.outline, _ty: PhantomData })
    }
}
```

Why: base-db's slot must hold no tree salsa's LRU let go of, and reading it must take no
lock (docs/v2/plan/design/concurrency.md): a `Weak` upgrade is an atomic, and the edit comes
from the setter, so `reparse_onto` does no second prefix/suffix scan. `SourceFile::parse_file`
and `parsing::kept` (a `Mutex` per lookup, strong parses) have no caller outside `syntax`;
base-db's `EditionedFileId::parse` does not use them, so format may remove them.

> applied by requests (format is DONE): `Parse::reparse_onto(&self, delete, text: impl Into<Text>, edition)` beside `reparse_to` (which is now its unchanged-text check, `edit_between` and a call to it; `Text` for `Arc<str>`, as the crate takes texts now), `Parse::downgrade` and `pub struct WeakParse<T>` with `upgrade` at the crate root, as given. Tests in `syntax/src/tests.rs`: `a_reparse_onto_a_known_edit_is_the_whole_parse` (full and outline) and `a_weak_parse_upgrades_to_the_same_parse_while_the_tree_is_held`. `parse_file` and `parsing::kept` left in place (format may remove them; nothing asks it now).

## db-expand-inputs2: a dependency's `priv` flag, and the target tuple and `has_thumb_interworking` (cargo, crates/load-cargo/src/v2/crates.rs, crates/blaze-units/src/stdlib/target.rs, crates/project-model/src/toolchain_info/target_data.rs)

What: cargo fills the new base-db fields of db-expand tasks 19 and 23. Both are in the crate's
content id already.

1. `crates/load-cargo/src/v2/crates.rs:278`, `dependency`: cargo passes `--extern priv:name`
   for a dependency whose unit-graph `public` is `Some(false)` (`-Zpublic-dependency`):

```rust
let private = dep.public == Some(false);
Ok(BuiltDependency::with_prelude(name, krate, prelude, on.is_std && !unit.is_std).with_private(private))
```

   (`Dependency::with_private(self, bool) -> Self` and `is_private(&self) -> bool`,
   `crates/base-db/src/input.rs`.) Test: a unit whose dep has `public: Some(false)` gives a
   dependency with `is_private()`; `None` and `Some(true)` do not.

2. `crates/blaze-units/src/stdlib/target.rs`, beside the `data_layout` of request 12 in
   `v2/requests/cargo.md`: `pub fn has_thumb_interworking(triple: &str) -> Result<bool, String>`,
   the recorded spec's `"has-thumb-interworking"` (`#[serde(rename = "has-thumb-interworking",
   default)] has_thumb_interworking: bool` on `Spec`; rustc's spec JSON leaves out a field that
   is false, so a missing key is `false`), with the same error as `data_layout` for a triple
   with no recorded spec. None of the three recorded specs (`rustc.json` `target_spec_json`)
   has the key today; recording an ARM spec makes it true there.

3. `crates/load-cargo/src/v2/crates.rs:94`, `workspace_data`:

```rust
target: Ok(TargetData {
    tuple: Symbol::intern(triple),
    data_layout: data_layout.into_boxed_str(),
    arch,
    has_thumb_interworking: target::has_thumb_interworking(triple)
        .map_err(|what| LowerError::at(what, triple))?,
}),
```

4. `crates/project-model/src/toolchain_info/target_data.rs:49` (compiles against the struct
   until project-model is removed): add `#[serde(rename = "has-thumb-interworking", default)]
   has_thumb_interworking: bool` to its `TargetSpec`, and to the literal
   `has_thumb_interworking: target_spec.has_thumb_interworking` and `tuple`: `target` when
   given, else the `host:` line of the same tool's `-vV` (the spec JSON has no tuple; its
   `llvm-target` differs from it on Apple targets, so it is not the tuple), as
   `intern::Symbol::intern(..)`.

Why: `TargetData` (`crates/base-db/src/target.rs`) now has `tuple: Symbol` and
`has_thumb_interworking: bool` (`DOMAIN` "blaze v2 target data 2"): rustc's
`UnsupportedInstructionSet` names the tuple and is given only without thumb interworking
(diagnostics' request "diagnostics-items-attrs"). Every `TargetData { .. }` literal without
them stops compiling.

> forwarded to cargo (told by tmux).

## db-expand-inputs2: `TargetData`'s two new fields at its other constructors (coordinator, crates/blaze-wasm/src/pack.rs, crates/blaze-wasm/src/lib.rs, crates/test-fixture/src/lib.rs)

What: each literal gains `tuple` and `has_thumb_interworking` (the wasm target has no thumb):
- `crates/blaze-wasm/src/pack.rs:204` and `crates/blaze-wasm/src/lib.rs:303`:
  `TargetData { tuple: Symbol::intern("wasm32-unknown-unknown"), data_layout: .., arch: Arch::Wasm32, has_thumb_interworking: false }`
  (`intern::Symbol`; use the tuple the crate already names for its target if it has one).
- `crates/test-fixture/src/lib.rs:266`:
  `TargetData { tuple: Symbol::intern("x86_64-unknown-linux-gnu"), arch: target_arch, data_layout: target_data_layout, has_thumb_interworking: false }`.
Why: as the section above; these do not compile without the fields.

> applied by requests for `crates/test-fixture/src/lib.rs` (the literal as given). `crates/blaze-wasm/src/pack.rs` and `lib.rs` forwarded to the coordinator (blaze-wasm is its; on requests' coordinator list).

## db-expand-inputs2: `Db::resolve_path` across source roots, and one global allocator in base-db's tests (frontend-db, crates/base-db/src/db.rs, crates/base-db/src/db/tests.rs)

What:
1. `crates/base-db/src/db.rs:203` `Db::resolve_path` copies the old own-root-only lookup. Make it
   `SourceDatabase::resolve_path`'s (`crates/base-db/src/lib.rs`), which also finds a file
   another source root names (db-expand task 22, a `#[path]` module outside its package),
   tracked through `crate::named`:

```rust
pub fn resolve_path(&self, path: AnchoredPath<'_>) -> Option<FileId> {
    let root = self.file_source_root(path.anchor).source_root_id(self);
    let root = self.source_root(root).source_root_ref(self);
    if let Some(file) = root.resolve_path(path) {
        return Some(file);
    }
    let mut target = root.path_for_file(&path.anchor)?.clone();
    target.pop();
    crate::named::resolve(self, &target.join(path.path)?)
}
```

   `named::resolve` takes `<Db: ?Sized + SourceDatabase>` and calls only
   `NamedPaths::try_get`, `file_source_root` and `source_root`; when `SourceDatabase` becomes
   `Db`'s inherent methods, change its bound to `db: &crate::Db`. `Files`' setters already
   publish the index (`writer.named.publish(db)`), so `Db`'s setters need nothing more.
2. `crates/base-db/src/db/tests.rs:8` declares a `#[global_allocator]`, and so does
   `crates/base-db/src/tests.rs:32`: both are in base-db's one test binary once `mod db;` is
   in `lib.rs`, which does not compile. Drop the one in `db/tests.rs`
   (`blaze_alloc_count::measure` works with the crate's).
Why: without 1, a crate lowered through `Db` misses a `#[path]` module outside its package
quietly (no-fallback).

> applied by frontend-db (both). Then db-expand-inputs2 replaced `named::resolve`, whose
> `SourceDatabase` bound `Db` does not meet, with `named::lookup<Db: ?Sized + salsa::Database>`.
> Each caller (`Db::resolve_path` in `db.rs` and `SourceDatabase::resolve_path` in `lib.rs`)
> now reads the found file's source root itself, with three lines in each.
> `Named::publish` is generic over `salsa::Database` too, so nothing in `named.rs` or
> `reparse.rs` names a `dyn` database.

> applied by frontend-db: 1 in `crates/base-db/src/db.rs` (`Db::resolve_path`, the body above);
> `named::resolve`'s bound becoming `db: &crate::Db` is in frontend-db's base-db request to db
> (`v2/requests/db.md`, "frontend-db: the concrete database"). 2: `db/tests.rs` declares no
> allocator now and measures with the crate's.

## db-expand-inputs2: `hot_lint.py`'s `crates` is a `list[str]`, which typed_input cannot read (coordinator, tools/v2/hot_lint.py)

What: `tools/v2/hot_lint.py:39` `crates: list[str] = field(default_factory=lambda: list(HOT))`
makes `{"root": ".", "crates": ["base-db"]}` (common.md's command) raise `TypeError:
typed_input: crates has a type it does not read: list[str]`; typed_input reads
`tuple[str, ...]`. Make it `crates: tuple[str, ...] = HOT`.
Why: no agent can run the lint as common.md says; base-db was linted by calling `lint_file`
directly.

> forwarded to the coordinator: `tools/` is its; on requests' coordinator list.

## frontend-tokens: token trees carry the parser's words (item 4)

Why: item 4, design in docs/v2/plan/lanes/frontend/tokens.md, "Item 4". A token tree will keep
the parser's word for each token, so that no expansion or fragment is converted to parser input.
Three call sites in hir-expand change when the matching tt, syntax-bridge and mbe parts land.
I will add the new entries next to the old ones first, so nothing breaks before then:

1. A macro call's input (`macro_arg` and its kin, now `syntax_node_to_token_tree` on the call's
   token tree): `syntax_bridge::call_input`, which writes the tree from the file's flat tokens.
2. A declarative macro's template: `mbe::DeclarativeMacro::parse_macro_rules` / `parse_macro2`
   take the expander's edition (`DeclarativeMacroExpander::edition`, which `apply_mark` gives
   every transcribed template token), so the template's words are made once. In
   `declarative.rs` that means computing `edition` before `mac`.
3. Trees built by hand (built-ins, fixups, attributes) and proc-macro answers: built with an
   edition function (`|ctx| ctx.edition(db)`), their one conversion.

I will tell you each signature here before it lands. Answer with `>` lines if any of these
can't take an edition function or a `FlatTree` there.

> forwarded to db-expand (hir-expand is db's; told by tmux).
> left for db-expand-macros-next2 (frontend-hooks, 2026-10-29): its edits are in `crates/hir-expand/`,
> all of which the running db-expand-macros-next2 owns; not in yet. Told by tmux.

## db-expand-macros: persist `ExpandErrorKind::Builtin` (db-expand-names, `crates/hir-def/src/nameres/frozen.rs`)

What: encode `ExpandErrorKind::Builtin(Box<BuiltinError>)` (message, code, notes, helps,
`also_at` spans, `derive`) as a variant of its own, as the other seven.
Why: it falls into `_ => Err("a macro error")` today, so a frozen def map loses every builtin
macro's rustc-worded error (`Mbe` has the same gap).

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand-names: `frozen.rs` writes `Builtin` (tag 8: message, code, `also_at` spans, notes and helps in order, `derive`) and `ProcMacroPanic` (tag 9); the match is exhaustive, so a new `ExpandErrorKind` variant won't compile until it is encoded. `Mbe` is still refused with an error naming it (its `mbe::ExpandErrorKind` has no stable form yet). `ENCODING` is "... 10". Test `a_builtin_macro_error_reads_back_whole`.

## db-expand-macros: `hir::MacroError` carries a builtin error whole (db-ty-next1, `crates/hir/src/diagnostics.rs`)

What: give `MacroError` the rendered kind, the error code, the notes and helps, and the extra
primary spans (`BuiltinError::also_at`) from `RenderedExpandError`/`BuiltinError`.
Why: `from_derive`'s two-span errors and `CoercePointee`'s `E0802` can't be shown otherwise.

> forwarded to db (from requests, pass 16; told by tmux).

## db-expand-macros: builtin errors without blaze's prefix (diagnostics, `crates/blaze-diagnostics/src/lower/macro_error.rs`)

What: for kinds `builtin-macro-error` and `builtin-derive-error` (`RenderedExpandError::BUILTIN`,
`BUILTIN_DERIVE`), give the message as is (no "blaze can't expand macro `x`:"), with the code,
the notes/helps and the extra spans of the previous section.
Why: these texts are `rustc_builtin_macros`' now, quoted exactly; rustc's
`tests/ui/macros/macros-nonfatal-errors.stderr` is the reference.

> forwarded to diagnostics (from requests, pass 16; told by tmux).

## db-expand-macros: the macro-error tests' expectations (diagnostics, `crates/ide-diagnostics/src/handlers/macro_error.rs`)

What: update the tests expecting "expected string literal", "failed to load file" and
"`OUT_DIR` not set, build scripts may have failed to run" and their spans: `env!`/`include!`
errors are rustc's now (e.g. "environment variable `X` not defined at compile time" with
rustc's help), spanned from the macro path to the closing delimiter.

> forwarded to diagnostics (from requests, pass 16; told by tmux).

## db-expand-macros: whether `builtin-macro-error` fails a build (engine, `crates/blaze-engine`)

What: decide whether `builtin-macro-error` joins the SURE list beside `builtin-derive-error`.
Why: every builtin-macro error is now one rustc gives as a hard error; derive errors keep the
derive kind for this reason.

> forwarded to codegen (`crates/blaze-engine`) (from requests, pass 16; told by tmux).

## db-expand-macros: an include's read error and resolved path (db-expand-inputs2, `crates/base-db/`)

What: let `include!`/`include_str!`/`include_bytes!` get the I/O error and the resolved path of
a file they can't read.
Why: rustc says "couldn't read `{path}`: {error}"; blaze can only say "failed to load file".

> forwarded to db (from requests, pass 16; told by tmux).

## db-expand-macros: what macro expansion waits on (format, `crates/mbe/`, `crates/span/`, `crates/hir-def/src/{item_tree,expr_store}/`)

1. Task 13: `mbe::ExpandErrorKind` (`crates/mbe/src/lib.rs:75`) carries the best failure's
   account (the failing token, the remaining matcher, whether a comma makes a rule match), so
   `ExpandErrorKind::Mbe` and then `hir::MacroError` carry it.
2. Task 15.1: a `dropped_semicolon: Option<Span>` field in `span::SpanMap` (the expansion map),
   filled from `token_tree_to_syntax_node`'s third value, so
   `semicolon_in_expressions_from_macros` reads it without expanding twice.
3. Task 15.3: `item_tree/lower.rs` (~786) and `expr_store/expander.rs` (~116) call
   `ExpandTo::of_call_site` and report its `Err(kind)` (no-fallback); `collect_macro_as_stmt`
   (`expr_store/lower.rs`) takes an `Expr` expansion for a block's non-braced tail call. Then
   `of_call_site` makes that tail `Expr` (rustc's `parse_stmt_mac`) and `from_call_site`, which
   takes an unknown parent as items, goes.
4. Task 16: the transcriber wraps `stmt`/`block` fragments in an invisible group spanned by the
   metavariable (`crates/mbe/src/expander/transcriber.rs:431-433`) and the span map records each
   group's range; then `map_node_range_up*` give such a node the group's span.
5. Task 17: the transcriber (`transcriber.rs:297-385`) returns the spans of the identifiers
   `${concat(..)}` makes; then they are kept with the expansion.

> forwarded to frontend-tokens (`mbe`) and db (`span`, hir-def), both told (from requests, pass 16; told by tmux).

## db-expand-macros: cfg_select's and the parser's errors in rustc's words (diagnostics-items, `crates/blaze-diagnostics/src/lower/parse.rs`)

What: `diagnostics-items-resolve.md` section 4: `cfg_select!`'s parse errors use the parser
translation, so they wait on it being usable on a token stream.

> forwarded to diagnostics (from requests, pass 16; told by tmux).

## speed-hir-sol-defs: hot_lint and budgets

From `speed-hir-sol-defs` (round 2 of `speed-hir`; report `/tmp/blaze-lanes/v2-speed-hir-sol-defs.md`).
Every edit below is mechanical: old text, new text, why. Line numbers are as of 10-29 20:30 and
move; the old text is what to find. `&dyn SourceDatabase` findings (204 of the 331 in these files)
are left to frontend-db-def's `&Db` rewrite and are not here. `Symbol`, `Name` and `ModPath` are
`Copy` now, so every `.clone()` of one is a copy written `*x`. A syntax cursor's clone
(`syntax-tree/src/cursor.rs`) is a non-atomic count bump that allocates nothing, which is the
reason given where one is justified.

> forwarded to db (from requests, pass 16; told by tmux).

### 1. The crate's allocation-budget harness (first: other tests already depend on it)

`signatures.rs`, `hir/columns.rs`, `item_tree/items/tests.rs`, `nameres/facts/tests/*.rs`,
`deps/signatures/tests.rs` and `expr_store/tests/alloc.rs` call `blaze_alloc_count` today, so
hir-def's test binary does not compile without (a), and with (b) but not (c) it has two global
allocators. Until (b), `blaze_alloc_count::measure` counts nothing in this binary, so every
`assert_at_most(0, ..)` passes vacuously. This is speed-hir-store's request in
`v2/requests/db.md` ("one counting allocator for hir-def's tests"), forwarded to db and not yet
applied; it is in your files, so here it is again with my test module.

a. `crates/hir-def/Cargo.toml`, `[dev-dependencies]`, after `expect-test.workspace = true`:
   ```toml
   blaze-alloc-count.workspace = true
   ```
   (`blaze-alloc-count` is in the root `[workspace.dependencies]` already.)
b. `crates/hir-def/src/lib.rs`, after
   ```rust
   #[cfg(test)]
   mod test_db;
   ```
   add
   ```rust
   #[cfg(test)]
   mod alloc_budgets;

   #[cfg(test)]
   #[global_allocator]
   static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
   ```
   `alloc_budgets.rs` is new and written (by me): budgets of 0 for an attribute-flag read, a gated
   value read with and without its attribute, a loc intern hit, a lang-item lookup and an upvars
   read.
c. `crates/hir-def/src/nameres/tests/paths.rs` (db-expand-names' file): delete
   ```rust
   #[global_allocator]
   static ALLOCATOR: alloc_count::Counting = alloc_count::Counting;
   ```
   and the whole `mod alloc_count { .. }` after it, and at its one use
   `let (res, allocations) = alloc_count::count(resolve);` becomes
   `let (res, used) = blaze_alloc_count::measure(resolve);` and `assert_eq!(allocations, 0);`
   becomes `assert_eq!(used.allocations, 0);`.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-expand-names: (c). `nameres/tests/paths.rs` has no global allocator now; until (a) and (b) land, hir-def's test binary has none counting, so that budget passes vacuously (and the binary needs (a) to compile at all, as the other `blaze_alloc_count` callers already do).

### 2. `crates/hir-def/src/attrs.rs`

Fixes:
- `parse_repr_tt`, `packed` (~L2055) and `align` (~L2076): the token tree is cloned to call
  `tts.next()` after a peek. `next_if` takes it by value, so no clone. Same tokens consumed.
  ```rust
                let pack = if let Some(NodeOrToken::Node(tt)) = tts.peek() {
                    let tt = tt.clone();
                    tts.next();
                    let mut tt_iter = TokenTreeChildren::new(&tt);
  ```
  becomes
  ```rust
                let pack = if let Some(NodeOrToken::Node(tt)) =
                    tts.next_if(|it| matches!(it, NodeOrToken::Node(_)))
                {
                    let mut tt_iter = TokenTreeChildren::new(&tt);
  ```
  and
  ```rust
                if let Some(NodeOrToken::Node(tt)) = tts.peek() {
                    let tt = tt.clone();
                    tts.next();
                    let mut tt_iter = TokenTreeChildren::new(&tt);
  ```
  becomes
  ```rust
                if let Some(NodeOrToken::Node(tt)) =
                    tts.next_if(|it| matches!(it, NodeOrToken::Node(_)))
                {
                    let mut tt_iter = TokenTreeChildren::new(&tt);
  ```
- `derive_info` (~L1872): `Box::new([])` becomes `Box::default()` (an empty `Box<[Symbol]>`,
  no allocation either way; the lint reads `Box::new(`).

Justify, a comment on the line above each:
- ~L901 `let mut literal = value.clone();`:
  `// clone: a cursor, no allocation; only for an item whose flag says it has the attribute`
- ~L949 `.with_value(ast::VariantDef::cast(variant_syntax.value.syntax().clone()).unwrap());`:
  `// clone: a cursor, no allocation`
- ~L1124 `let text = format!("#[cfg{}] fn f() {{}}", group.syntax().text());`:
  `// alloc: only a `#[link(.., cfg(..))]`, once per extern block, inside its memo`
- ~L1253 `owner: &dyn HasAttrs,`:
  `// dyn: `ast::attrs_including_inner` takes `&dyn HasAttrs`; one call per field or variant`
- ~L1317 `let mut libs = Vec::new();` (`native_libs`):
  `// alloc: the memo's value; nothing for an extern block without `#[link]``
- ~L1595 `return Vec::new();` (`target_feature_list`):
  `// alloc: an empty `Vec`, no allocation`
- ~L1597 `let mut result = Vec::new();` (`target_feature_list`):
  `// alloc: only a function with `#[target_feature]`; the caller owns the list`
- ~L1694 and ~L1705 `let mut result = Vec::new();` (`doc_aliases`, `fields_doc_aliases`):
  `// alloc: the memo's value, only for an item whose flag says `#[doc(alias)]``
- ~L1735 and ~L1751 `let mut result = Vec::new();` (`cfgs`, `fields_cfgs`):
  `// alloc: the memo's value, only for an item whose flag says `#[cfg]``
- ~L1949 `let mut result = Vec::new();` and ~L1962 `let mut segments = Vec::new();`
  (`define_opaque`): `// alloc: the memo's value, only for a function with `#[define_opaque]``
- ~L2124 `.collect()` (`parse_rustc_legacy_const_generics`):
  `// alloc: the memo's value, only for a function with `#[rustc_legacy_const_generics]``
- ~L2181 `.collect();` (`next_doc_expr`, `DocAtom::Alias`):
  `// alloc: `#[doc(alias(..))]`, read only inside the gated doc-alias memo and docs`

> forwarded to db (from requests, pass 16; told by tmux).

### 3. `crates/hir-def/src/attrs/docs.rs`

Docs are read by `blaze-doc` and `hir` only, never by a build (`rg 'AttrFlags::(docs|field_docs)'`).
A comment on the line above each:
- ~L289 `let mut lines: Vec<(&str, TextSize)> = doc` (flagged at its `.collect();`, ~L295), ~L297
  and ~L303 `let raw_lines: Vec<&str> = ..collect();`, ~L563 `Some(lines.first()?[..i].to_string())`,
  ~L834 `docs: String::new(),`, ~L835 `docs_source_map: Vec::new(),`, ~L898
  `if result.is_empty() { None } else { Some(Box::new(result)) }`:
  `// alloc: docs, read by blaze-doc and hir, never by a build` (for ~L295 put it on the line
  above `.collect();`, inside the chain)
- ~L468 `self.docs = String::from_utf8(bytes)`:
  `// alloc: none: `from_utf8` keeps `bytes`' buffer`
- ~L632 `let expr = parse.clone().cast::<ast::Expr>().map(|parse| parse.tree())?;`:
  `// clone: docs (or a `#[key = macro!()]` value): one count bump per expanded call`
- ~L686, ~L688 (`extend_with_attrs`) and ~L827, ~L828 (`extract_docs`), the `&dyn Fn() -> ..`
  parameters: `// dyn: docs, one call per item's docs`
- ~L754 `let body = ast::Fn::cast(owner.syntax().clone())?.body()?;` and ~L762
  `Some(body.stmt_list()?.syntax().clone())`: `// clone: a cursor, no allocation`

> forwarded to db (from requests, pass 16; told by tmux).

### 4. `crates/hir-def/src/attrs/stability.rs`

- ~L358 `let mut fields = Vec::new();` (`own_of_fields`):
  `// alloc: the memo's value, once per variant whose fields' stability is asked`
- ~L563 `tt.syntax().children_with_tokens()..collect();` (into the `SmallVec<[_; 16]>`):
  `// alloc: a 16-element stack buffer; spills only for a longer attribute list`
- ~L584 `ast::String::cast(value.clone())..`, ~L588 `items.push((key.clone(), value));`, ~L677
  `InFile::new(ast_id.file_id, ast_id.to_node(db).syntax().clone())`, ~L739 and ~L747
  `..ast_id(&ast::Variant::cast(node.value.clone())?)..` / `ast::Item::cast(..)`:
  `// clone: a cursor, no allocation`

> forwarded to db (from requests, pass 16; told by tmux).

### 5. `crates/hir-def/src/lang_item.rs`

Fixes (`Symbol` is `Copy`):
- ~L163 and ~L189: `current.scope(db).type_(&Name::new_symbol_root(module.clone()))` becomes
  `current.scope(db).type_(&Name::new_symbol_root(*module))`.
- ~L217: `trait_items.associated_type_by_name(&Name::new_symbol_root(name.clone()));` becomes
  `trait_items.associated_type_by_name(&Name::new_symbol_root(*name));`
- ~L223: `trait_items.method_by_name(&Name::new_symbol_root(name.clone()));` becomes
  `trait_items.method_by_name(&Name::new_symbol_root(*name));`
- ~L339: `if traits.is_empty() { None } else { Some(traits.into_iter().collect()) }` becomes
  `if traits.is_empty() { None } else { Some(traits.into_boxed_slice()) }` (same value, no second
  collect).

Justify:
- ~L113 `if lang_items.is_empty() { None } else { Some(Box::new(lang_items)) }`:
  `// alloc: the memo's value, once per crate`
- ~L325 `let mut traits = Vec::new();`: `// alloc: the memo's value, once per crate`

> forwarded to db (from requests, pass 16; told by tmux).

### 6. `crates/hir-def/src/lib.rs`

Fixes (`ModPath` is `Copy`):
- ~L1672: `let def = resolver(path).ok_or_else(|| UnresolvedMacro { path: path.clone() })?;`
  becomes `let def = resolver(path).ok_or(UnresolvedMacro { path: *path })?;`
- ~L1697: `_ => return Err(UnresolvedMacro { path: path.clone() }),` becomes
  `_ => return Err(UnresolvedMacro { path: *path }),`

Justify:
- ~L1667 `eager_callback: &mut dyn FnMut(`:
  `// dyn: hir-expand's `EagerCallBackFn`; one call per eager macro call`
- ~L1784 `.collect()` (`include_macro_invoc`): `// alloc: the memo's value, once per crate`

> forwarded to db (from requests, pass 16; told by tmux).

### 7. `crates/hir-def/src/src.rs`

Fixes: `fd` is the loop's own value, borrowed only by `is_cfg_enabled_for` before:
- ~L164 `Either::Left(fd.clone()),` becomes `Either::Left(fd),`
- ~L179 `Either::Right(fd.clone()),` becomes `Either::Right(fd),`

Justify (both map a `use`'s trees back to syntax, for diagnostics and the IDE crates):
- ~L46 `use_tree_source_map(db, use_ast_id)[index].clone()`:
  `// clone: a cursor, no allocation`
- ~L73 `InFile::new(loc.id.file_id, use_tree_source_map(db, loc.id).into_iter().collect())`:
  `// alloc: a use's trees back to syntax, for diagnostics`

> forwarded to db (from requests, pass 16; told by tmux).

### 8. `crates/hir-def/src/diagnostic_item.rs`

Fixes (`Symbol` is `Copy`):
- ~L49 `self.duplicates.push((it.key().clone(), *it.get(), owner));` becomes
  `self.duplicates.push((*it.key(), *it.get(), owner));`
- ~L104 `result.insert(name.clone(), owner);` becomes `result.insert(*name, owner);`

Justify:
- ~L86 `let mut result = own.clone();`:
  `// clone: the memo's value starts as the crate's own map, once per crate`
- ~L91 `let mut queue = vec![krate];`: `// alloc: the crates' queue, once per crate`

> forwarded to db (from requests, pass 16; told by tmux).

### 9. `crates/hir-def/src/upvars.rs`

Justify (once per body with a closure; the frame allocator takes them once `blaze-mem` has one):
- ~L120 `binding_names: store.bindings().map(..).collect(),`:
  `// alloc: once per body with a closure, sized by its bindings`
- ~L123 `mentioned: Vec::new(),`: `// alloc: once per body with a closure that captures`
- ~L131 `Some(Box::new(result))`: `// alloc: the memo's value, once per body with a capture`

> forwarded to db (from requests, pass 16; told by tmux).

### 10. `crates/hir-def/src/unstable_features.rs`

- ~L52 `crate::crate_def_map(db, krate).features().clone()`:
  `// clone: once per def-map change of the crate; the copy lets its readers backdate`

> forwarded to db (from requests, pass 16; told by tmux).

### 11. `macro_expansion_tests/` and `test_db.rs`

Both are `#[cfg(test)] mod ..;` in `lib.rs`, so all of it is test code, but hot_lint can't see
that from inside the files, and most of the 43 findings in `macro_expansion_tests/` are inside
fixture strings, where a comment would change the test's input and the spans its expectations
print. Nothing to do here: I asked speed (`v2/requests/speed.md`, "speed-hir-sol-defs") for hot_lint
to skip the files of a module declared `#[cfg(test)]`. `test_db.rs` becomes
`pub(crate) type TestDB = base_db::Db;` in frontend-db-def's rewrite anyway.

> forwarded to db (from requests, pass 16; told by tmux).

### Check after applying

`PYTHONPATH=python/typed-input/src python3 tools/v2/hot_lint.py '{"root": "."}' | rg 'crates/hir-def/src/(attrs|lang_item|lib|src|diagnostic_item|upvars|unstable_features)'`
shows only `dyn SourceDatabase` lines until frontend-db-def's rewrite lands, then nothing.

> forwarded to db (from requests, pass 16; told by tmux).

### 12. Attributes are read off the live parse, so every body edit re-runs them (design)

Counted in `docs/v2/plan/lanes/db/speed-of-light.md`, section "hir-def: attributes, interned
locs, lang items (speed-hir-sol-defs)".
- What happens: `attrs_source` reads the item through `loc.source(db)`, which reads `parse` and
  `ast_id_map`, and both change on every edit of the file. The signature queries read
  `AttrFlags::query`, `repr` and `repr_assume_has` (`signatures.rs` 612, 714, 822, 929,
  1024). So verifying them after a body edit re-runs the flag query of every item in the file,
  along with `fields_attrs` and `query_generic_params`.
- Cost: ≈ 150 × 300 ns ≈ 45 µs per edited file, all backdated, against ≈ 0.8 µs if the queries
  depended only on `file_outline`.

The change, in order of payoff:
1. `attrs_source` reads an item that is in its file's `file_outline` from the outline (the
   lookup `item_syntax` does: `outline.ast_id_map.try_get_erased`, then `to_node` on the
   outline root), and reads every other item (inside a body, or written by a macro) as it does
   now. For this, `bodies_under` in `signatures.rs` must not cut a body whose text, after
   trivia, starts with `#!`, since an inner attribute is part of its item's attributes. Test:
   in `alloc_budgets.rs`'s style, edit a fn body in a two-item file and assert, with
   `executed_in_read_order`, that no `AttrFlags::query` runs.
2. Walk `syntax::view` nodes, borrowed from the memo, rather than `SyntaxNode`s: this drops the
   per-item root `Box` and the write to the file tree's `Arc` count, a line every worker on the
   file shares. It needs a view form of `hir_expand::attrs::expand_cfg_attr`.
3. (format's, in `span`) `AstIdMap` keeps each entry's element index, so `to_node` is one load
   instead of a binary search over the file's elements.
4. `file_attr_flags(db, file) -> &[AttrFlags]`, built in one walk of the outline, with
   `AttrFlags::query` as a tracked read of one entry. `crate_lang_items` then scans the
   columns' `LANG_ITEM` bits instead of running every item's query on one thread (≈ 7.5 ms
   for `core` when it is the first reader).

Status: open

> forwarded to db (from requests, pass 16; told by tmux).

## speed-hir-sol-nameres: hot_lint and budgets

From `speed-hir-sol-nameres` (round 2 of `speed-hir`; report
`/tmp/blaze-lanes/v2-speed-hir-sol-nameres.md`). The bounds behind the two design edits (1, 2)
are in `docs/v2/plan/lanes/db/speed-of-light.md`, "hir-def: name resolution
(speed-hir-sol-nameres)". Files: db-expand-names' (`nameres.rs`, `nameres/` but `facts/`,
`item_scope.rs`, `resolver.rs`) and db-expand's (`find_path.rs`, `import_map.rs`, `lib.rs`).
`nameres/facts/` is in `v2/requests/frontend-nameres.md` under the same heading. Line numbers are
as of 10-29 21:10 and move (`frozen.rs` moved 110 lines while I read it); the old text is what
to find. Left out:
- the `&dyn SourceDatabase` findings, which frontend-db-def's `&Db` rewrite covers;
- `collector.rs`'s 66 findings, which go away once frontend-nameres-core's `#[cfg(test)] mod
  collector;` (`v2/requests/db.md`) lands.

`Name` and `Symbol` are `Copy`, so their `.clone()` becomes `*x`. A syntax node's clone is a
reference-count bump that allocates nothing.

**With `v2/requests/mem-db-def-collect.md` (frame collections, exact boxes).** Where it rewrites a
line I asked you to justify, apply its version and skip my comment:
- in `nameres.rs`: A3 (`DefMap::empty`'s `diagnostics` becomes `Box::default()`), A5
  (`registered_tools`), A6 (`DiagnosticsByModule::of`; keep my comment only on its
  `(0..len).collect()`) and A7 (`ModuleScope::traits`);
- D1 (`assoc.rs`), D2 (`uses.rs`) and D3 (`demand.rs`'s `found`, `calling`, `seen`, `writing`
  and `words`).

My fixes and design edits don't change frame types or boxes:
- the `Name`/`Symbol` copies and `served`'s clone stay right under D5's `served_in`;
- `ModuleData::collected` keeps A8's 16-byte `ScopeSlot` and works with D5's generic `Dec`;
- `Modules::name` is a new `Tag::Nameres` query that allocates nothing;
- `Resolver::scopes` as a `SmallVec` is outside every query frame, as its rules require.

The counting allocator is asked for twice: mem-db-def's H3 (`v2/requests/mem-db-def.md`) and
§1 a–c of speed-hir-sol-defs above. They are the same change. Apply it once, because a second
`#[global_allocator]` doesn't build.

> forwarded to db (from requests, pass 16; told by tmux).

### 0. Budget tests: `crates/hir-def/src/nameres_budgets.rs` (new, written by me)

It needs the counting allocator wired into hir-def's test binary first: items a–c of
speed-hir-sol-defs' section above. Without them it doesn't link, and with only some of them it
counts nothing. Then, in `crates/hir-def/src/lib.rs`, next to that section's
`#[cfg(test)] mod alloc_budgets;`, add:
```rust
#[cfg(test)]
mod nameres_budgets;
```
The budgets, each measured once the reads are warm:
- 0 for a memo hit of `crate_def_map`, of `ModuleId::def_map` and of every block's
  `block_def_map`;
- 0 for `ItemScope::get` in a scope of at most 8 names and in one of more, and for the same
  lookups through `ModuleData::name_in_scope`'s per-name memo;
- 0 for `resolve_module_path_in_items` of paths of two and three segments, one of them through a
  renaming import (`nameres/tests/paths.rs`' `resolved_path_allocates_nothing` already covers
  `resolve_path_fp_with_macro`);
- 0 for `Resolver::update_to_inner_scope` into a block that has a block def map, looking up a
  local, a block item and `a::b::g` in the value namespace, then `reset_to_guard`;
- at most 1 for building `FunctionId`'s resolver. Lower it to 0 in
  `a_function_resolver_allocates_at_most_one` when edit 2 lands.

> forwarded to db (from requests, pass 16; told by tmux).

### 1. `crates/hir-def/src/nameres.rs`

**Design: one memo per path segment.** A segment read today costs two memo hits:
`Modules::module` (≈ 11 ns), then `ModuleScope::name` (≈ 11 ns). This edit makes it one, about
115 → 70 ns for a 3-segment path in this crate. The dependencies stay per module and per name.
- In `#[salsa::tracked] impl<'db> Modules<'db>`, after `fn module(..)`, add:
  ```rust
      /// What `name` means in `module`: one memo for a path segment, where reading the module
      /// and then its scope is two. Depends on the modules there are and on that module's scope;
      /// backdated while the name means what it did.
      #[salsa::tracked]
      fn name(self, db: &'db Db, module: ModuleId, name: Name) -> PerNs {
          let Some(data) = self.table(db).get(&module) else {
              panic!("ModuleId not found in ModulesMap: {module:#?}")
          };
          data.scope(db).get(&name)
      }
  ```
- In `impl DefMap`, after `pub fn module<'a>(..)`, add:
  ```rust
      /// What `name` means in module `id` of this `DefMap`; depends on that name only.
      pub(crate) fn name_in_module(&self, db: &Db, id: ModuleIdLt<'_>, name: &Name) -> PerNs {
          match &self.modules {
              ModulesSlot::Building(_) => self.module(db, id).name_in_scope(db, name),
              ModulesSlot::Collected(modules) => {
                  // SAFETY: as in `Self::module`: the `'static` copy is only the key of the
                  // `Modules::name` query, hashed and compared, never read through.
                  let id = unsafe { id.to_static() };
                  *<Modules<'_> as salsa::plumbing::FromId>::from_id(*modules).name(db, id, *name)
              }
          }
      }
  ```
  frontend-nameres-core asks for a `ModulesSlot::Unit { own, rows }` in `v2/requests/db.md`
  (its §4, "a resolver view"). Once that lands, the first arm becomes
  `ModulesSlot::Building(_) | ModulesSlot::Unit { .. } => ..`. A view's segment reads stay on
  `DefMap::module`, which reads a row as a fact. Its `ScopeSlot::Fact` (§5 there) needs nothing
  here: `Modules::name` reads through `ModuleData::scope`, which handles every slot. Then use
  `name_in_module` in `path_resolution.rs` (§3).

**Fixes:**
- `with_ancestor_maps`: `f: &mut dyn FnMut(&DefMap, ModuleIdLt<'db>) -> Option<T>,` becomes
  `mut f: impl FnMut(&DefMap, ModuleIdLt<'db>) -> Option<T>,`. It is called once per path from
  `find_path`. The callers pass `&mut |..| ..`, which is `impl FnMut` too, so they compile
  unchanged.
- `name_in_scope`: `.name(db, name.clone())` becomes `.name(db, *name)`.
- `legacy_macro_in_scope`: `.legacy_macro(db, name.clone())` becomes `.legacy_macro(db, *name)`.
- `ModuleSource::node`:
  ```rust
          match self {
              ModuleSource::SourceFile(it) => it.syntax().clone(),
              ModuleSource::Module(it) => it.syntax().clone(),
              ModuleSource::BlockExpr(it) => it.syntax().clone(),
          }
  ```
  becomes
  ```rust
          let node = match self {
              ModuleSource::SourceFile(it) => it.syntax(),
              ModuleSource::Module(it) => it.syntax(),
              ModuleSource::BlockExpr(it) => it.syntax(),
          };
          // clone: an owned handle for the caller, a count bump
          node.clone()
  ```
- **Decoding a frozen map builds the collected scope directly.** Today `ModuleData::with_scope`
  boxes a `BuildingScope` per module, and `hand_over_scopes` then takes it apart again. Add,
  after `with_scope`:
  ```rust
      /// A module of a `DefMap` whose scope is final (a frozen `DefMap` read back): the scope
      /// goes to the database at once, with no `BuildingScope` in between.
      pub(crate) fn collected(
          db: &Db,
          id: ModuleId,
          origin: ModuleOrigin,
          visibility: Visibility,
          parent: Option<ModuleId>,
          scope: ItemScope,
          children: FxIndexMap<Name, ModuleId>,
      ) -> Self {
          let scope = ModuleScope::new(db, id, scope, children);
          ModuleData {
              origin,
              visibility,
              parent,
              scope: ScopeSlot::Collected(salsa::plumbing::AsId::as_id(&scope)),
          }
      }
  ```
  `hand_over_scopes` already skips a `Collected` slot. `with_scope` stays for
  `facts/macros.rs` (`ModuleData::with_scope(` ~L919) and `facts/collect.rs` (~L826). Its
  `Box::new(BuildingScope { .. })` gets, on the line above:
  `// alloc: one box per module of a def map being built, moved to the database when it is done`.
  The `frozen.rs` side is in §4.

**Justify** (the comment goes on the line above):
- `DiagnosticsByModule::of`, before `let mut order: Vec<u32> = (0..len).collect();` and before
  `let mut modules: Vec<(ModuleId, u32)> = Vec::new();`:
  `// alloc: once per def map, when collection is done; the memo's own columns`.
- `DefMapCrateData::new`, `registered_tools: PREDEFINED_TOOLS.iter()..collect(),`:
  `// alloc: once per crate`.
- `ModuleScope::traits`, `self.scope(db).traits().collect()`: `// alloc: the memo's value, once
  per module scope`.
- `crate_local_def_map`'s `Arc::new(DefMapCrateData::new(krate.edition)),`:
  `// alloc: once per crate`.
- `block_def_map`'s `local_def_map.def_map(db).data.clone(),`: `// clone: an Arc count bump,
  once per block def map`.
- `DefMap::empty`'s `diagnostics: Vec::new(),`: `// alloc: empty; allocates nothing until a
  diagnostic`.
- `dump`'s `let mut buf = String::new();` and `let path = format!(..)`: `// alloc: a debug dump
  (the edge)`.

> forwarded to db (from requests, pass 16; told by tmux).

### 2. `crates/hir-def/src/resolver.rs`

**Design: the scope stack inline.** `Resolver` is built per body walk and per IDE query. Its
first `push` allocates a `Vec`, and a body's walk usually grows it once more. The bound is
25 ns and 0 allocations, against 85 ns and 1–2 allocations today.
- `scopes: Vec<Scope<'db>>,` becomes `scopes: SmallVec<[Scope<'db>; 8]>,`. `Scope` is `Copy`
  and at most 24 B, so a `Resolver` grows by about 200 B. `smallvec::SmallVec` is imported
  already.
- In the doc comment, "pushes its scopes onto the same `Vec`" becomes "pushes its scopes onto
  the same stack".
- `impl HasResolver for ModuleId`: `let mut scopes = Vec::new();` becomes
  `let mut scopes = SmallVec::new();`.
- Every other use works on `SmallVec` unchanged: `push`, `reserve`, `len`, `truncate`,
  `[start..].reverse()`, `iter().rev().copied()`, `scopes.reverse()` and the struct literal. If
  any `resolver_for_*` builds the field from a `Vec`, `SmallVec::from_vec` takes it without a
  copy.

**Fixes:**
- `.map(|(name, module_id)| (name.clone(), module_id.0))` (`names_in_scope`, extern prelude)
  becomes `.map(|(name, module_id)| (*name, module_id.0))`.
- `ScopeNames::add` and `add_local`: `self.map.entry(name.clone()).or_default();` becomes
  `self.map.entry(*name).or_default();`, in both.

> forwarded to db (from requests, pass 16; told by tmux).

### 3. `crates/hir-def/src/nameres/path_resolution.rs` (with §1's design edit)

- `resolve_remaining_segments`:
  ```rust
                      let module_data = if module.block(db) == self.block_id() {
                          self.module(db, module)
                      } else {
                          module.def_map(db).module(db, module)
                      };

                      // Since it is a qualified path here, it should not contains legacy macros
                      reads(PathRead::Slot(module, *segment));
                      module_data.name_in_scope(db, segment)
  ```
  becomes
  ```rust
                      let def_map = if module.block(db) == self.block_id() {
                          self
                      } else {
                          module.def_map(db)
                      };

                      // Since it is a qualified path here, it should not contains legacy macros
                      reads(PathRead::Slot(module, *segment));
                      def_map.name_in_module(db, module, segment)
  ```
- `resolve_name_in_module`: `let from_scope = self.module(db, module).name_in_scope(db, name);`
  becomes `let from_scope = self.name_in_module(db, module, name);`.
- `resolve_name_in_crate_root_or_extern_prelude`:
  `def_map.module(db, def_map.root).name_in_scope(db, name)` becomes
  `def_map.name_in_module(db, def_map.root, name)`, and
  `self.module(db, self.root).name_in_scope(db, name)` becomes
  `self.name_in_module(db, self.root, name)`.
- `resolve_in_prelude_reading`: `def_map.module(db, prelude).name_in_scope(db, name)` becomes
  `def_map.name_in_module(db, prelude, name)`.

Results are the same: `Modules::name` reads the scope `name_in_scope` reads. The `PathRead`s
are unchanged.

> forwarded to db (from requests, pass 16; told by tmux).

### 4. `crates/hir-def/src/nameres/frozen.rs`

**Fixes:**
- `served`: `let frozen = FrozenDefMaps::try_get(db)?.maps(db).get(&krate)?.clone();` becomes
  `let frozen = FrozenDefMaps::try_get(db)?.maps(db).get(&krate)?;`. `Mode` is `Copy`,
  `&frozen.bytes` is a `&Arc<[u8]>` that derefs to `&[u8]`, and the rest of the function reads
  fields only. This drops a count bump per served crate.
- `Enc::unplaced`: `Ok(if unplaced == written { text.clone() } else { Symbol::intern(&unplaced) })`
  becomes `Ok(if unplaced == written { *text } else { Symbol::intern(&unplaced) })`, because
  `text: &Symbol`.
- In the decoder's module loop (~L3102):
  ```rust
              let parent = parent.map(|it| self.own[it as usize]);
              let module = ModuleData::with_scope(origin, visibility, parent, scope, children);
  ```
  becomes
  ```rust
              let parent = parent.map(|it| self.own[it as usize]);
              let module = ModuleData::collected(
                  self.db,
                  self.own[index],
                  origin,
                  visibility,
                  parent,
                  scope,
                  children,
              );
  ```
  This makes the same `ModuleScope`s, with the same keys, in the same order as
  `hand_over_scopes` (§1). It drops one `Box` per decoded module and the `mem::take` of its
  scope.

**Justify** (the comment goes on the line above each). Line numbers are from one hot_lint run
with the file unchanged during it:
- Error paths, `// alloc: error path`. These are the `Err(format!(..))`,
  `.ok_or_else(|| format!(..))`, `"..".to_owned()` inside `Err` and `.map_err(..)` lines: 196,
  266, 272, 350, 358, 540, 546, 556, 661, 758, 780, 1615, 1639, 1764, 1788, 1791, 1807, 1832,
  1839, 2068, 2080, 2104, 2121, 2134, 2139, 2144, 2174, 2190, 2208, 2226, 2235, 2241, 2264,
  2297, 2324, 2338, 2362, 2364, 2388, 2412, 2429, 2440, 2445, 2461, 2466, 2473, 2489, 2494,
  2504, 2507, 2517, 2530, 2554, 2568, 2605, 2612, 2618, 2650, 2657, 2687, 2697, 2706, 2722,
  2832, 2836, 2916, 2946, 2957, 2975, 3034, 3038, 3079, 3128, 3142, 3145, 3166, 3183, 3210.
- `Mode::Check` and `Mode::Verify` bookkeeping, `// alloc: Check/Verify only, which record
  differences; never when serving`: 109 (`Arc::new(maps)` in `set`), 147 (`DIFFERENCES`), 151,
  173, 174, 216, 310.
- Setting the frozen inputs, once per load, `// alloc: once per load of the frozen bytes`:
  238 (`Arc::new(maps)` in `set_lang_items`) and 275 (`Ok(Box::new(items))`, the memo value).
- Encoding, `// alloc: encoding, once per crate per image write (not on a build's path)`: the
  `Enc` lines 326, 327, 356, 379, 385, 394, 395, 397, 404, 414, 490, 494, 502, 519, 520, 526,
  558, 561, 634, 640, 645, 665, 668, 745, 940, 1310, 1481, 1564, 1720, 1822, 1836, 1868.
- Decoding, `// alloc: the decoded map's own storage, once per crate served`: the `Dec` lines
  2029–2032 (`Dec::new`'s columns), 2050, 2156, 2179, 2184, 2272, 2287, 2471, 2974, 2984,
  3020, 3118, 3135, 3150–3152, 3220.

> forwarded to db (from requests, pass 16; told by tmux).

### 5. `crates/hir-def/src/nameres/demand.rs`

**Fixes:**
- `.iter().cloned()` at ~L432 and in `let mut writing: FxHashSet<Symbol> =
  found.iter().cloned().collect();` (~L566) becomes `.iter().copied()`.
- `if seen.insert((name.clone(), id)) {` becomes `if seen.insert((*name, id)) {`. If `name` is
  a `Name` by value there, it is `(name, id)`.
- In `let named = scope.macros().map(|(name, it)| (name.symbol().clone(), it.def));` and
  `.flat_map(|(name, ids)| ids.iter().map(move |&id| (name.symbol().clone(), id)));`,
  `name.symbol().clone()` becomes `*name.symbol()`.
- `found.push(name.clone());` and `writing.insert(name.clone());` become `found.push(*name);`
  and `writing.insert(*name);`.

**Justify:**
- The `Err(format!(..))` lines (~L234, 244, 303, 309, 320): `// alloc: error path`.
- `let mut node: SyntaxNode = block.to_node(db).syntax().clone();`, the
  `ast::Variant::cast(ancestor.clone())`, and the four `.map(|it| it.syntax().clone())` in the
  body-of-an-owner match (~L487–496): `// clone: a node handle, a count bump`.
- The owned results, `// alloc: the query's result, once per execution (per crate or per
  body)`: `let mut owners = Vec::new();` (×3), `let blocks: Vec<_> = Body::of(..).collect();`,
  `let mut seen: FxHashSet<DefWithBodyId> = ..collect();`, `let mut bodies = Vec::new();` (×2),
  `let mut found = Vec::new();` and `let mut calling = Vec::new();` (×2 each),
  `let mut names: Vec<Symbol> = known.into_iter().collect();`, `let mut words = Vec::new();`
  and the `writing` set.

> forwarded to db (from requests, pass 16; told by tmux).

### 6. `crates/hir-def/src/item_scope.rs`

**Justify:**
- `Index { slots: std::iter::repeat_n(EMPTY, slots).collect() }`: `// alloc: the index's
  slots, once per scope of more than 8 names, when it is sealed`.
- `Column { entries: self.entries.clone(), of: PhantomData }`: `// clone: only with its
  ItemScope, which the facts engine copies once per macro step (frontend-nameres's
  copy-on-write rows remove it)`.
- `BUILTIN_SCOPE`'s `let names: Box<[Name]> = ..collect();` and
  `BuiltinScope { names, defs: defs.collect(), index }`: `// alloc: once per process
  (LazyLock)`.
- `self.all_names().map(|(ns, _, name)| (ns, name)).collect();` (a `SmallVec` of at most 8):
  `// alloc: inline, at most 8 names; no heap`.
- `self.all_names().map(|(ns, pos, _)| ns << 30 | pos as u32).collect();` and the `by_def`
  `.collect();` (~L1393): `// alloc: once per scope, on the first sorted or by-def read
  (OnceLock)`.
- `dump`'s `let mut entries: Vec<_> = self.resolutions().collect();` and
  `let mut legacy_macros = self.legacy_macros().collect::<Vec<_>>();`: `// alloc: a debug dump
  (the edge)`.

> forwarded to db (from requests, pass 16; told by tmux).

### 7. `crates/hir-def/src/find_path.rs`

**Fixes:**
- `return Some(Choice::new(st, kind, name.clone(), Stable));` becomes
  `return Some(Choice::new(st, kind, *name, Stable));`.
- `(declared || !ignore_local_imports).then(|| name.clone())` becomes
  `(declared || !ignore_local_imports).then(|| *name)`.
- `Some(Choice::new(st, PathKind::Plain, name.clone(), Stable))` becomes
  `Some(Choice::new(st, PathKind::Plain, *name, Stable))`.

The rest of the file (the `name.clone()`s from ~L2564 on) is test code. dedup-codec-rng has an
edit in those tests, so the three fixes above don't touch it.

> forwarded to db (from requests, pass 16; told by tmux).

### 8. `crates/hir-def/src/import_map.rs`

**Superseded** by mem-db-def-items' I6 (`v2/requests/mem-db-def-items.md`). It moves the build
onto the query frame with its own `// alloc:` comments, and replaces the FST search with sorted
keys, so the search allocates nothing. Apply I6 and skip this block. What follows applies only
if I6 doesn't land.

**Justify**, as `// alloc: once per ImportMap::of execution (per crate, on the IDE's import
search)`: `let mut out = String::new();` (~L87), `vec![0u32; rows + 1]`, `vec![0u32;
found.infos.len()]`, the `order.iter().map(|&at| found.infos[at as usize].1.clone()).collect()`
(with `// clone:` saying the same), `vec![0u64; rows.div_ceil(64)]`,
`let mut importables: Vec<u32> = ..collect();`, `let mut key = Vec::new();` and
`let mut worklist = vec![root];`.
The crate-list `..map(|dep| ImportMap::of(db, dep.crate_id)).collect()` (~L572) gets
`// alloc: once per search request (the edge)`.
`dump`'s three `format!("- {it:?} ..")` and its `.collect();` get `// alloc: a debug dump
(the edge)`.

> forwarded to db (from requests, pass 16; told by tmux).

### 9. Smaller files under `nameres/`

- `proc_macro.rs`: `name: func_name.clone()` (×3) becomes `name: *func_name`.
  `Some((trait_name.as_name(), Box::new([])))` becomes
  `Some((trait_name.as_name(), Box::default()))`, the same empty box with no allocation.
- `mod_resolution.rs`, `let candidate_files: Box<[String]> = match attr_path {`: put
  `// alloc: once per out-of-line `mod` while collecting; the not-found diagnostic keeps them`
  on the line above each of `let mut path = String::new();`, `Box::new([path])` and
  `Box::new([format!("{dir}{name}.rs"), format!("{dir}{name}/mod.rs")])`.
- `assoc.rs`:
  - `DefDiagnostics::new(vec![]),` gets `// alloc: empty; allocates nothing`;
  - `items: Vec::new(),` and `diagnostics: Vec::new(),` in the collector's constructor get
    `// alloc: the query's result, once per trait or impl`.
- `attr_resolution.rs`, `let mut tt = tt.clone();`: `// clone: the attribute macro call's own
  argument (its delimiter is changed), once per call`.
- `uses.rs` (being written): `uses: Vec::new(),`, `unresolved: Vec::new(),`,
  `let mut ambiguous = Vec::new();` and `let mut todo = Vec::new();` get `// alloc: once per
  execution, the result's storage`. The `globs: ..collect(),` gets `// alloc: the result`. If
  `todo` is a worklist that runs per module, `SmallVec<[_; 8]>` keeps it inline instead.

> forwarded to db (from requests, pass 16; told by tmux).

### Check after applying

`PYTHONPATH=python/typed-input/src python3 tools/v2/hot_lint.py '{"root": "."}' | rg 'crates/hir-def/src/(nameres|item_scope|resolver|find_path|import_map)'`
shows only `collector.rs` (until it is `#[cfg(test)]`), `facts/` (frontend-nameres's) and the
`dyn SourceDatabase` lines until frontend-db-def's rewrite. `blaze test -p hir-def
nameres_budgets` passes.

Status: open

> forwarded to db (from requests, pass 16; told by tmux).

## frontend-tokens: a macro's input from a view, `syntax_bridge::call_input` (item 4)

Why: item 4 (docs/v2/plan/design/frontend.md) says a macro call's input is read from the
file's flat tokens, with no syntax handle. Item 16 (frontend-tree) makes views the way to read
syntax.

What: `syntax_bridge::call_input(node: syntax::view::Node<'_>, map, call_site, mode) ->
tt::TopSubtree` is in `crates/syntax-bridge/src/lib.rs`. It is `syntax_node_to_token_tree` on a
view: the same one pass over the flat tree's tokens, through lazy parts, with spans from `map`
in order. `a_views_call_input_is_the_cursors_token_tree` holds it to the cursor's walk.

Exact edits, both in `crates/hir-expand/src/lib.rs`, `MacroCallId::macro_arg`, the
`MacroCallKind::FnLike` arm (the one input made for every fn-like call of a build):

1. Old:

```rust
            MacroCallKind::FnLike { ast_id, .. } => {
                let node = &ast_id.to_ptr(db).to_node(&root);
```

New:

```rust
            MacroCallKind::FnLike { ast_id, .. } => {
                use syntax::view::AstNode as _;
                let node = ast_id.to_view(db);
```

The rest of the arm reads the same names on the view (`path`, `syntax().text_range()`,
`token_tree`, `left_delimiter_token`, `right_delimiter_token`, a token's `kind`), all in
`syntax::view` (`node_ext.rs:1057,1064`, `generated/nodes.rs:1052,1054`). `to_view` reads the
tree `parse_with_map` gives `root` from (`HirFileId::root`, `lib.rs:1905`), so the spans of `map`
are of the same tokens.

2. Old:

```rust
                let mut tt = match loc.def.kind {
                    MacroDefKind::Declarative(..) | MacroDefKind::ProcMacro(..) => {
                        syntax_bridge::syntax_node_to_token_tree(
                            tt.syntax(),
                            MacroRulesArgs(InOrder::new(map)),
                            span,
                            mode,
                        )
                    }
                    _ => syntax_bridge::syntax_node_to_token_tree(
                        tt.syntax(),
                        InOrder::new(map),
                        span,
                        mode,
                    ),
                };
```

New:

```rust
                let mut tt = match loc.def.kind {
                    MacroDefKind::Declarative(..) | MacroDefKind::ProcMacro(..) => {
                        syntax_bridge::call_input(
                            tt.syntax(),
                            MacroRulesArgs(InOrder::new(map)),
                            span,
                            mode,
                        )
                    }
                    _ => syntax_bridge::call_input(tt.syntax(), InOrder::new(map), span, mode),
                };
```

Left as they are, on purpose:
- `declarative.rs:136, 160, 167` (a definition's arguments and body): once a definition, and
  `syntax_node_to_token_tree` of a flat node already takes the same pass (`FlatConverter`), so a
  view saves one handle a definition.
- `lib.rs:922, 935, 987, 1007`: `expand_speculative`, IDE only, on a node that may be an edited
  green tree, which only the cursor reads.

mbe now takes `db: &Db` (`DeclarativeMacro::expand`, `expect_fragment`); hir-expand's callers
already pass `&Db`, so they need no edit.

`syntax_node_to_token_tree_modified` (cfg stripping and fixup: `cfg_process.rs:319`,
`fixup.rs`) stays on handles, because it removes and appends elements by `SyntaxElement`. Its
view version needs a view of a removal set, which is frontend-tree's to design. I'll take that
up with them.

The tree's parser words are made lazily, at the first fragment a matcher tries. A proc macro's
input never pays for them. Nothing built.

> forwarded to db (from requests, pass 17; told by tmux).
> left for db-expand-macros-next2 (frontend-hooks, 2026-10-29): its edits are in `crates/hir-expand/`,
> all of which the running db-expand-macros-next2 owns; not in yet. Told by tmux.

## speed-hir: four edits in db-expand's files (dedup, upvars, `ModPath`; 2026-10-29)

speed-hir's children are done; these were forwarded to speed-hir after them and the files are
yours again. Each is mechanical; the full edits are written out in `v2/requests/speed-hir.md`.

1. `crates/hir-def/src/import_map.rs`, `import_map_query_impl`: the hand-written counting sort
   becomes `blaze_ids::radix::Csr::build` (one counting sort in the tree). Edit as written in
   `v2/requests/speed-hir.md`, "## dedup: `import_map.rs`'s counting sort onto
   `blaze_ids::radix::Csr`". Checked: `Csr::build(rows, pairs)`, `items()` and `into_parts()`
   exist (`crates/blaze-radix/src/csr.rs:47,100,129`), `blaze_ids::radix` re-exports
   `blaze_radix`, and hir-def depends on `blaze-ids`. Same order, same bytes.
2. `crates/base-db/src/table.rs`: `Table`'s per-shard leveled tables become one
   `intern::swiss::Index` per shard, and `fn shard_count` becomes `blaze_ids::shard_count()`.
   Edit as written in `v2/requests/speed-hir.md`, "## dedup: base-db's `Table` onto
   `blaze_mem::swiss::Index`", steps 1-8. Checked: `Index::new`, `find(hash, is)`,
   `insert(len, hash, value, hash_of)` (`crates/blaze-mem/src/swiss.rs:257,269,283`),
   `intern::swiss` (`crates/intern/src/lib.rs:21`), `blaze_ids::shard_count`
   (`crates/blaze-ids/src/lib.rs:326`); base-db depends on `intern` and `blaze-ids`.
3. `crates/hir-def/src/upvars.rs`, `upvars_mentioned_impl` (~L112), once mem-format adds
   `ExpressionStore::has_closures` (requested in `v2/requests/mem-format.md`, "## speed-hir"):
   old
   ```rust
       if !store.exprs().any(|(_, expr)| matches!(expr, Expr::Closure { .. })) {
   ```
   new
   ```rust
       if !store.has_closures() {
   ```
   Why: `exprs()` decodes a full `Expr` view (kind byte, data word, extra words) for every
   expression of every body to learn it has no closure, which most don't; `has_closures` is a
   byte compare over the `expr_kind` column (`memchr`-speed, ~200 bytes for a 200-expression
   body). `Expr` stays imported (the visitor matches on it).
4. `crates/hir-expand/src/mod_path.rs:34`: delete `intern::impl_internable!(ModPath);`. Every
   `Interned<ModPath>` is gone (`rg 'Interned<ModPath>' crates` finds none, including
   `crates/hir/src/semantics.rs`); `ModPath` is 16 bytes and `Copy`, stored by value. Removing
   the impl drops its sharded intern table from every database and makes a reintroduced
   second intern a compile error.

> forwarded to db (from requests, pass 17; told by tmux).

## db-expand-names: trait imports method resolution used (db-ty, `crates/hir-ty/src/method_resolution/probe.rs`, `infer.rs`, `infer/path.rs`)

What: rustc's `used_trait_imports`. When a method call or a type-relative path (`Type::item`,
`<T>::item`) picks an item through a trait in scope (`CandidateKind::TraitCandidate`, from
`assemble_extension_candidates_for_traits_in_scope`, `probe.rs:1193`; not an inherent,
`ObjectCandidate` or `WhereClauseCandidate` pick, and not an operator's
`lookup_method_for_operator`), record the trait and the expression or pattern:
1. `InferenceResult` gains `pub traits_in_scope_used: Box<[(ExprOrPatId, TraitId)]>`, in the
   order inference made the picks (no sort, so its bytes are the same each run), each pair once.
   Fill it where `method_resolutions` is filled for a `TraitPick` that came from a
   `TraitCandidate`, and in `infer/path.rs` `resolve_ty_assoc_item` for the same kind of pick.
   Persist it with the rest of `InferenceResult` (`infer/persist.rs`).
2. A tracked query in hir-ty, `used_imports_with_traits(db: &Db, krate: Crate) ->
   hir_def::nameres::uses::UsedImports` (`returns(ref)`): for each body owner of the crate
   (`hir_def::nameres::demand::crate_body_owners(db, krate)`), for each
   `(at, trait_)` of its inference result, build the resolver at `at`
   (`hir_def::resolver::resolver_for_scope(db, owner.into(), ExprScopes::of(db, owner).scope_for(expr))`,
   the expression's for a pattern), call `resolver.record_trait_uses(db, trait_, &mut uses)`, then
   return `hir_def::nameres::uses::used_imports_with(db, krate, &uses)`.
Why: without it, `use m::Tr;` used only by `x.method()` and a `use foo::prelude::*;` read only
for its traits look unused (rustc keeps them: `TraitCandidate::import_ids` from
`find_transitive_imports`, marked by `lookup_method` and `resolve_fully_qualified_call`). The
hir-def half is in place: `Resolver::record_trait_uses` gives the import of the candidate rustc
picks (the impl's own trait none; the innermost block scope, then the module; the std
prelude none) and follows globs to the binding each read, `use Tr as _` through a glob included;
`used_imports_with` merges those uses into the same walk `used_imports` reads, so nothing is walked
twice. Tests of the hir-def half: `nameres/tests/uses.rs` (`a_method_of_...`, `the_impl_s_own_trait_uses_no_import`,
`a_trait_of_the_std_prelude_uses_no_import`). For diagnostics: `lints/names/imports/used.rs`
then reads `hir_ty::used_imports_with_traits` in place of `used_imports`.

> forwarded to db (db-ty-next2, which holds hir-ty's inference; the hir-def half to db-expand-names-next2) (requests, pass 18; told by tmux).

## db-expand-names: an unnamed trait binding names its own import (frontend-nameres, `crates/hir-def/src/nameres/facts/`)

What: `ItemScope::unnamed_trait_imports` is now `ThinVec<(TraitId, Item<(), ImportOrExternCrate>)>`
and records the binding's own import (the `use Tr as _`, or the glob that read it from another
scope), as `collector.rs`'s `update_recursive` now does. Before, it was the source scope's
binding's import. Exact edits:
1. `facts/macros.rs` (~L1077, the `None =>` arm over `resolutions`):
   `let (tr, import) = match res.take_types_full() {` / `Some(Item { def: ModuleDefId::TraitId(tr), vis: _, import }) => (tr, import),`
   becomes
   `let tr = match res.take_types_full() {` / `Some(Item { def: ModuleDefId::TraitId(tr), vis: _, import: _ }) => tr,`
   and below, `self.scope_mut(module).push_unnamed_trait(tr, vis, import.and_then(ImportOrExternCrate::import));`
   becomes `self.scope_mut(module).push_unnamed_trait_through(tr, vis, import);`
   (`import` is then the function's own `Option<ImportOrExternCrate>`, as for the named arm).
2. `facts/unit.rs` `update_recursive` (~L931 and ~L962): the same two edits, with
   `self.scope_mut(module_id)`.
3. `facts/tests.rs` (~L691): `import: import.map(|it| self.import_id(it)),` becomes
   `import: import.map(|it| self.import_or_extern_crate(it)),`.
Why: a method found through a glob's unnamed trait marks that glob used (rustc's
`find_transitive_imports` walks `Import { import: glob, binding }`); with the old value the glob
was unknown. Until 1 and 2 land, facts and the collector disagree on these imports; the frozen
encoding is bumped (`ENCODING` "... 10") with it.

> forwarded to frontend-nameres (frontend-nameres-next1, holds `nameres/facts/`) (requests, pass 18; told by tmux).

## db-expand-names: a dumped `_` names its import (format, `crates/hir-def/src/expr_store/tests/body/block.rs`)

What: in `underscore_import` (~L428), `            - _ : type` becomes
`            - _ : type (import)`.
Why: `ItemScope::dump` prints an unnamed trait's import as it prints a named one's, now that the
scope records it (above).

> forwarded to mem-format (holds hir-def's `expr_store` files; format is DONE) (requests, pass 18; told by tmux).

## frontend-tokens: attribute and derive inputs from a view, `syntax_bridge::call_input_modified` (item 4)

Why: every attribute and derive macro's input goes through `attr_macro_input_to_token_tree`
(`crates/hir-expand/src/cfg_process.rs:303`), which walks handles (`PreorderWithTokens` over
`SyntaxNode`, one count a step) to strip cfgs. Item 4 (docs/v2/plan/design/frontend.md) wants
no syntax handle on a macro input's path.

What is ready (`crates/syntax-bridge/src/lib.rs`, after `syntax_node_to_token_tree_modified`):

```rust
pub fn call_input_modified<'t, SpanMap, OnEvent>(
    node: syntax::view::Node<'t>,
    map: SpanMap,
    append: FxHashMap<syntax::view::NodeId, Vec<tt::Leaf>>,
    remove: FxHashSet<syntax::view::NodeId>,
    call_site: Span,
    mode: DocCommentDesugarMode,
    on_event: OnEvent,
) -> tt::TopSubtree
where
    SpanMap: SpanMapper,
    OnEvent: FnMut(
        &mut syntax::view::PreorderWithTokens<'t>,
        &WalkEvent<syntax::view::Element<'t>>,
    ) -> (bool, Vec<tt::Leaf>),
```

It is `syntax_node_to_token_tree_modified` case for case (removed tokens, removed nodes with
their subtree skipped and their append taken, appends on leave, `on_event`'s drop, skip and
inserted leaves). `a_views_modified_input_is_the_cursors` (syntax-bridge `tests.rs`) holds it to
the handle version over the corpus, full and outline parses, with random removal and append sets
and an `on_event` that skips, drops and inserts.

What to change (db-expand's code; mechanical but long, so described, not diffed):
1. `cfg_process.rs` `macro_input_callback`: the returned closure takes
   `&mut syntax::view::PreorderWithTokens<'t>` and `&WalkEvent<syntax::view::Element<'t>>`;
   every `ast::X::cast(node.clone())` becomes `syntax::view::X::cast(node)` (views are `Copy`),
   and its attribute and cfg reads use the view accessors of the same names.
2. `fixup.rs` `fixup_syntax`: take `syntax::view::Node<'_>`; `SyntaxFixups::append` becomes
   `FxHashMap<syntax::view::NodeId, Vec<tt::Leaf>>` and `remove` `FxHashSet<syntax::view::NodeId>`,
   keyed by `element.id()` where `SyntaxElement`s are inserted now.
3. `attr_macro_input_to_token_tree` takes `node: syntax::view::Node<'_>` and calls
   `syntax_bridge::call_input_modified(node, InOrder::new(span_map), fixups.append, fixups.remove,
   span, DocCommentDesugarMode::ProcMacro, macro_input_callback(..))`. `macro_arg`'s Attr arm
   passes `ast_id.to_view(db).syntax()` (`AstId::to_view`, `files.rs:122`).
4. `expand_speculative` (IDE, a possibly edited green tree) keeps the handle path:
   `syntax_node_to_token_tree_modified` stays for it, and for `derive_macro.rs:1087`.

Nothing built.

> forwarded to frontend-hooks (frontend applies its own `## frontend-*` sections in db's files, db.md's notice of 20:45; it tells db-expand-macros-next1, which holds `crates/hir-expand/`) (requests, pass 18, corrected; told by tmux).
> left for db-expand-macros-next2 (frontend-hooks, 2026-10-29): its edits are in `crates/hir-expand/`,
> all of which the running db-expand-macros-next2 owns; not in yet. Told by tmux.

## dedyn: hir-def and hir-expand: generics for the small per-node callbacks, the rest justified

Each edit below is self-contained; call sites stay as they are unless said.

> forwarded to db by file (requests, pass 19; each told by tmux): `nameres.rs` to frontend-nameres-next1; `lib.rs`, `attrs.rs`, `attrs/docs.rs` to db-expand-next2; `expr_store/lower.rs` to mem-format (format's file, mem-format holds hir-def's store files); `item_scope.rs` to db-expand-names-next2; hir-expand `attrs.rs` to db-expand-macros-next1.
> applied by db-expand-macros (hir-expand `attrs.rs` subsection): the three `owner: &dyn ast::HasAttrs,` lines (`collect_item_tree_attrs`, `AttrId::find_attr_range_with_source`, `AttrId::find_attr_range_with_source_opt`) keep their `&dyn` and carry the `// dyn:` justification as written.

### crates/hir-def/src/nameres.rs: `DefMap::with_ancestor_maps`

Called per path search (find_path, five callers, each its own closure; a 15-line body: five
small instances).
before `        f: &mut dyn FnMut(&DefMap, ModuleIdLt<'db>) -> Option<T>,`
after  `        f: &mut impl FnMut(&DefMap, ModuleIdLt<'db>) -> Option<T>,`

### crates/hir-def/src/lib.rs: `macro_call_as_call_id` names hir-expand's justified alias

The callback is hir-expand's `EagerCallBackFn` (justified there: one call per macro call an
eager input holds; a generic would copy the recursive eager expander into each of the nine
callers). Spell it once:
before
```rust
    eager_callback: &mut dyn FnMut(
        InFile<(syntax::AstPtr<ast::MacroCall>, span::FileAstId<ast::MacroCall>)>,
        MacroCallId,
    ),
```
after
```rust
    eager_callback: EagerCallBackFn<'_>,
```
and in `use hir_expand::{ .. }`: `eager::expand_eager_macro_input,` becomes
`eager::{EagerCallBackFn, expand_eager_macro_input},`.

### crates/hir-def/src/expr_store/lower.rs: `check_cfg` and `may_have_attrs` per expression

`check_cfg` runs for every expression, statement, arm, field and parameter lowered;
`may_have_attrs` is its fast path (two virtual calls per node today). Both bodies are small
(15 and 4 lines) over about 15 node types, so generics; the full cfg evaluation
(`Expander::is_cfg_enabled` → `AttrFlags::is_cfg_enabled_for`) is reached only for a node that
carries an attribute and keeps its justified `&dyn` (the `&impl` coerces to it).
- before `    fn check_cfg(&mut self, owner: &dyn ast::HasAttrs) -> bool {`
  after  `    fn check_cfg(&mut self, owner: &impl ast::HasAttrs) -> bool {`
- before `pub(in crate::expr_store) fn may_have_attrs(owner: &dyn ast::HasAttrs) -> bool {`
  after  `pub(in crate::expr_store) fn may_have_attrs(owner: &impl ast::HasAttrs) -> bool {`
  (`tests/cfg_attrs.rs` passes `&ast::AnyHasAttrs`: unchanged.)

### crates/hir-def/src/attrs.rs: `AttrFlags::is_cfg_enabled_for` keeps its `&dyn`, justified

before `        owner: &dyn HasAttrs,`
after  `        owner: &dyn HasAttrs, // dyn: once per field, variant or node with an attribute; a generic would copy the cfg expansion per node type`

### crates/hir-def/src/attrs/docs.rs: `extend_with_attrs`, `extract_docs`

Two callers (`AttrFlags::docs`, `fields_docs`), each with its own closures: two instances of
the two functions (about 210 lines together). Call sites unchanged (`&|| ..`).
- in `extend_with_attrs`:
  `    get_cfg_options: &dyn Fn() -> &'a CfgOptions,` becomes
  `    get_cfg_options: &impl Fn() -> &'a CfgOptions,`, and
  `    make_resolver: &dyn Fn() -> Resolver<'db>,` becomes
  `    make_resolver: &impl Fn() -> Resolver<'db>,`
- in `extract_docs`:
  `    resolver: &dyn Fn() -> Resolver<'db>,` becomes `    resolver: &impl Fn() -> Resolver<'db>,`,
  and `    get_cfg_options: &dyn Fn() -> &'a CfgOptions,` becomes
  `    get_cfg_options: &impl Fn() -> &'a CfgOptions,`

### crates/hir-def/src/item_scope.rs: the dump's display name, justified

before `            let display_name: &dyn fmt::Display = match &name {`
after  `            let display_name: &dyn fmt::Display = match &name { // dyn: the debug dump, formatting text anyway`

### crates/hir-expand/src/attrs.rs: `&dyn ast::HasAttrs`, justified

These three take an item-tree owner once per item and hand it to syntax's
`ast::attrs_including_inner(&dyn HasAttrs)`; the vtable serves only `syntax()` and the trait's
default methods. A generic would copy the cfg_attr expansion per node type, and the concrete
`ast::AnyHasAttrs` costs a node refcount per call, no cheaper. Put on each of the three
`owner: &dyn ast::HasAttrs,` lines (in `collect_item_tree_attrs`,
`AttrId::find_attr_range_with_source`, `AttrId::find_attr_range_with_source_opt`):
`// dyn: once per item, into syntax's attrs_including_inner; a generic would copy the cfg_attr expansion per node type`

## db-expand-macros: what tasks 16 and 17 still wait on, and `Mbe` in the frozen def map (format, `crates/mbe/`, `crates/span/`; db-expand-names, `crates/hir-def/src/nameres/frozen.rs`)

State at 10-29 21:10. Format's best failure (task 13), `SpanMap::dropped_semicolon` (15.1) and the
`of_call_site` callers (15.3) are in, and hir-expand uses them. Still missing:

1. Task 16 (format, `crates/mbe/src/expander/transcriber.rs` `expand_var`, `crates/span/src/map.rs`):
   - `stmt` and `block` fragments are `Fragment::Tokens`, pushed with `extend_with_tt_alone` and no
     group. Wrap them, as `Fragment::Expr`/`Path`/`Ty` are, in an invisible group spanned by the
     metavariable (`id` after `marker`).
   - `SpanMap::push_fragment` keeps only a fragment's range. An invisible delimiter has no text, so
     its span is in no piece of the map. Keep the group's span beside its range (for example
     `push_fragment(range, span)` and `fragment_span(range) -> Option<Span>` for a range that is
     exactly one fragment's), and keep it through `merge`.
   - Then hir-expand's `map_node_range_up`, `map_node_range_up_rooted` and
     `map_node_range_up_rooted_span` give a node whose range
     is exactly a fragment's the fragment's span. Why: rustc lints a `$x:stmt`'s statement at the
     metavariable (`suggest-remove-semi-in-macro-expansion-issue-142143.rs`).
2. Task 17 (format, `transcriber.rs` `Op::Concat`): return the spans of the identifiers
   `${concat(..)}` makes, for example in the `ExpandResult`'s value beside `matched_arm`, or as a
   `SpanMap` column. hir-expand then keeps them with the expansion. Why: the lexer lints
   (`diagnostics-lints-syntax-early-lexer`) skip a made identifier, and only the transcriber knows
   which ones it made.
3. db-expand-names: `frozen.rs` refuses `ExpandErrorKind::Mbe` because mbe's kind had no stable
   form. `mbe::ExpandErrorKind` is now a closed enum (`BindingError`, `UnresolvedBinding`,
   `LeftoverTokens`, `LimitExceeded`, `NoMatchingRule`, `UnexpectedToken`, and
   `MatchFailure { token, remaining, remaining_span, missing_comma }`). Please encode it, so a
   frozen def map keeps a `macro_rules!` call's error rather than refusing the map.

## frontend-hooks: `FileRange::file_text` returns `&syntax::Text` (db-expand-macros, `crates/hir-expand/src/files.rs`; 2026-10-29)

From frontend-hooks, which applied base-db's side of frontend-text's "the parse tree's source is
a `Text`" (`v2/requests/db.md`): `FileText::text` returns `&'db syntax::Text` now
(`crates/base-db/src/lib.rs`). `crates/hir-expand/src/files.rs:80` names the old type:

- old: `    pub fn file_text(self, db: &Db) -> &std::sync::Arc<str> {`
- new: `    pub fn file_text(self, db: &Db) -> &syntax::Text {`

The body and its one caller, `&self.file_text(db)[self.range]` (L86), are unchanged
(`Text: Deref<Target = str>`); nothing else calls it (`rg '\.file_text\(' crates`). hir-expand
already depends on `syntax`. Without it `files.rs` doesn't compile against base-db.

> applied by db-expand-macros: `crates/hir-expand/src/files.rs` `FileRange::file_text` returns `&syntax::Text`, as written. hir-expand's other file-text reads (`builtin/fn_macro.rs` `line_and_column`, `line_starts`, `source_joined`, the include bytes) go through `Text`'s `Deref<Target = str>` unchanged.

## db-expand-macros: `Code for triomphe::Arc<T>`, landing with `Attr::input` (format, `crates/hir-def/src/hir/persist.rs`)

What: beside `impl<T: Code> Code for Box<T>` (`:658`), with the same bodies:

```rust
impl<T: Code> Code for triomphe::Arc<T> {
    fn put(&self, w: &mut Words) {
        (**self).put(w);
    }

    fn get(r: &mut Read<'_>) -> Result<Self, FormatError> {
        T::get(r).map(triomphe::Arc::new)
    }
}
```

Why: `hir_expand::attrs::Attr::input` is now `Option<triomphe::Arc<AttrInput>>` (frontend-deps-reparse's
request above, applied in `crates/hir-expand/src/attrs.rs`), and `item_tree/persist.rs`'s `Code for Attr`
puts and gets `self.input`. hir-def does not compile until this impl is in; the bytes written don't change.
Nothing else needs editing: the builders in `item_tree/attrs.rs` use `.into()` (triomphe has `From<T>`),
and the readers (`pretty.rs:102`, `attr_resolution.rs:121`, `attrs.rs`) go through `as_ref`/`as_deref`.
