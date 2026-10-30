# Requests from db-salsa

## `Output: Hash` for every `lru` tracked function without `no_eq` (db-expand, db-ty)

What: a tracked function with an `lru` option and without `no_eq` now requires its return type to
implement `std::hash::Hash`. The macro emits `Fingerprint::of_value(value)` for it, so a missing
impl is a compile error at the return type. The bound, exactly:

- `Output<'db>: Hash`, where the hash feeds everything `PartialEq` compares, and nothing
  address-dependent. A `Hash` that hashes a pointer (`intern::Symbol`, `intern::Interned<T>`, an
  `Arc` by address, `triomphe::Arc::as_ptr`) is unsound here: the fingerprint outlives the value
  (that is its purpose), so after the value is evicted and the allocation is freed and reused, a
  different value can hash the same and be backdated. Salsa IDs (inputs, tracked, interned
  structs) hash index and generation and are fine.
- Either derive/implement such a `Hash`, or add `no_eq` to the function (then it is never
  backdated by value, and evicted values that recompute count as changed unless no input changed).

Where (as of this writing; `rg 'lru *=' crates`):

- db-expand:
  - `crates/base-db/src/editioned_file_id.rs:22` `parse` -> `syntax::Parse<ast::SourceFile>`
  - `crates/hir-expand/src/lib.rs:503` `parse_macro_expansion`
  - `crates/hir-expand/src/lib.rs:1775` `ast_id_map` -> `AstIdMap`
  - `crates/hir-def/src/attrs.rs:1316` `cfgs`, `:1328` `fields_cfgs`, `:1377` `docs`,
    `:1401` `fields_docs`
  - `crates/hir-def/src/expr_store/body.rs:94` `with_source_map`, `:144` `relative`
- db-ty:
  - `crates/hir-ty-mir/src/mir/lower.rs:4393`, `:4402`, `:4413`, `:4892` (the `mir_body*` queries)
  - `crates/hir-ty-mir/src/mir/transform.rs:161`, `:181`, `:189`, `:197`, `:205`, `:218`, `:232`,
    `:246` (the `runtime_mir_body*` and `monomorphized_runtime_mir_body*` queries)
  - `crates/hir-ty-codegen/mod.rs:3079` `compiled_instance`. (`:4962` `inlinable` has `no_eq` and
    is exempt.)

Why: LRU eviction used to break early cutoff: an evicted value that recomputed equal could not be
compared (the old value was gone), so everything that read it ran again. The memo now keeps a
128-bit fingerprint of its value through eviction, and each read records the fingerprint it saw,
so a recomputed equal value is backdated and a reader whose input returned to the value it read
is not rerun (docs/v2/plan/lanes/db.md; issues #350, #806, #734). Functions without `lru` are unchanged: they
keep their values, so `PartialEq` still decides.

> applied: db-expand's sites, by db-expand (`v2/requests/db-expand.md`, the `Hash` section; `Parse` and `mbe::ValueResult` forwarded to format there).
> forwarded to format: `crates/hir-def/src/expr_store/body.rs:94` and `:144` are format's.
> forwarded to codegen: the "db-ty" sites are in `crates/hir-ty-mir/` and `crates/hir-ty-codegen/`, which are codegen's (crates.md).

## Call `evict_values` instead of relying on `trigger_lru_eviction` (blaze-engine, coordinator)

What: after a build, `crates/blaze-engine/src/lib.rs:7466` calls `db.trigger_lru_eviction()`,
which only trims each `lru` function to its cap. To free the large values (MIR bodies, compiled
instances) without losing early cutoff, call `<function>::evict_values(db)` for them; it returns
how many values it dropped. `<function>::evict_values_of(db, keys)` drops chosen keys of a function
whose one argument is a Salsa struct. Both exist only on functions with an `lru` option (`lru = 0`
means no cap, so values are kept until evicted this way).

Where: `crates/blaze-engine/src/lib.rs` near line 7466; the functions in `hir-ty-mir` and
`hir-ty-codegen` listed above.

Why: issues #806 and #734 (memory held by values no later build reads).

> forwarded to codegen: `crates/blaze-engine/src/lib.rs` is codegen's, not the coordinator's.

## Memory

Every memo grows by 8 bytes (`QueryRevisions` is 32 bytes, was 24): one `ThinVec` that is empty,
with no allocation, unless the memo has a value fingerprint or read an `lru` function. Then it
holds 16 bytes of header plus 20 bytes per such read and 20 for its own value fingerprint. No other change to the
build is needed.

> applied: note only.
