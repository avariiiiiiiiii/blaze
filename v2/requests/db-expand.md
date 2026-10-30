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

Why: the def-map collector expands each round's macros in parallel (docs/v2/db.md, "Parallel
macro expansion"); each worker thread reads through its own fork.

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
- `crates/hir-def/src/nameres/tests.rs:877` and `:886` (the parent's): `db.file_text(root)
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

Why: docs/v2/README.md section 2 and docs/v2/crates.md "IDs": results keyed by input hashes,
never by revision, so views with equal inputs share them; a setter that changes a value without
its ID would let a stale result be reused.
