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
