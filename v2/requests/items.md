# Requests from lane items

## items-facts: `InherentImpls::iter` (for `items-hirty`, `crates/hir-ty/src/method_resolution.rs`)

What: a public accessor on `InherentImpls` that lists every simplified self type with its impls:

```rust
impl<'db> InherentImpls<'db> {
    /// Every simplified self type with its inherent impls, in no particular order.
    pub fn iter(&self) -> impl Iterator<Item = (&SimplifiedType<'db>, &[ImplId])> + '_ {
        self.map.iter().map(|(self_ty, impls)| (self_ty, &**impls))
    }
}
```

Where: `crates/hir-ty/src/method_resolution.rs`, next to `InherentImpls::for_self_ty`.

Why: `Fact::Method { self_ty, name }` is keyed by the hash of a simplified type
(`blaze_items::SimplifiedType`), which can't be turned back into hir's `SimplifiedType`. The
fact's value is computed by grouping every crate's inherent impls by that hash
(`crates/blaze-items/src/index.rs`, `inherent_impls_in`), so it has to enumerate
`InherentImpls::for_crate`'s map, which is private and has no iterator. Collecting the impls
again outside hir-ty would not see the same body blocks (`impl_holders` is private), and
missing one is a stale result. `index.rs` calls `InherentImpls::for_crate(db, krate).iter()`
as written above; it does not compile until this lands. No behavior change: read-only.

## items-facts: `Unhashable` derives `Debug` (for `items`, `crates/blaze-items/src/hash.rs`)

`crates/blaze-items/src/tests/facts.rs` unwraps `Result<_, Unhashable>`, which needs
`Unhashable: Debug`.

## items-hirty: whole impl and trait item lists read outside items' files (for `db`)

What: four reads of an impl's or a trait's whole item list where one name, or only the
associated types, is used. Each makes adding an item of another name to an impl or trait
rerun every body that goes through it:

- `crates/hir-ty/src/next_solver/solver.rs:235-254`: normalizing an associated item of an impl
  reads `impl_id.impl_items(db)` whole and `find_map`s one name. Read a per-(impl, name) query
  instead, as `method_resolution::impl_value_item` is for functions and constants (an
  `impl_type_item(db, impl, name) -> Option<TypeAliasId>` beside it, same order: the first match).
- `crates/hir-ty/src/next_solver/interner.rs:1663`: `trait_items(..).associated_types()` reads
  the whole list for the associated types only. Use a per-trait query of the associated types,
  as `opaques::trait_type_aliases` (private to `opaques.rs`; I can make it `pub(crate)` if you
  want to call it).
- `crates/hir-ty/src/lower/path.rs:589` and `crates/hir-ty/src/infer/path.rs:302`: the same
  shape, one name looked up in a whole list.

Why: docs/v2/items.md 6.2, `Fact::Method`. With these four left, the narrowing in items' files
(`impl_value_item`, `trait_value_item`, `impl_type_aliases`) only takes effect for bodies whose
inference does not go through them. No behavior change: return the first match by name, as
now.

## items-hirty: `hir-ide` incremental expectations (for the coordinator)

`crates/hir-ide/src/tests/incremental.rs`, `add_struct_invalidates_trait_solve`, first
`expect![..]`: the method probe of `s.method()` now reads `trait_value_item` (one execution).
Expected: `"trait_value_item",` between `"InherentImpls < 'db >::for_crate_",` and
`"TraitImpls < 'db >::in_crate_and_deps_for_trait_and_self_ty_",`. The second `expect![..]`
should be unchanged if `TraitItems::query_with_diagnostics_` comes out equal after the edit,
which it does when the trait's `FunctionId`s are stable; if not, `"trait_value_item",` appears
once after `"TraitItems::query_with_diagnostics_",`. The positions are predicted, not run:
`UPDATE_EXPECT=1` settles them. No other test in that file probes methods, finds destructors,
lays out types or checks coherence.

## items: `cfg` and the other new dependencies of `crates/blaze-items/Cargo.toml` (for the coordinator)

What: `crates/blaze-items/Cargo.toml` now depends (all `workspace = true`, all already
workspace dependencies) on blaze-format, blaze-ids, blaze-query, serde, rustc-hash, salsa,
triomphe, either, rustc_type_ir, rustc_abi, base-db, cfg, hir-def, hir-expand, hir-ty, intern,
span, syntax; dev-dependencies hir, test-fixture, test-utils. `Cargo.lock` needs the entries.
Why: the hashes are computed over the front end's database (docs/v2/crates.md, `blaze-items`:
"it gains hir-ty dependencies").

## items: keep `test-fixture` (or what replaces it) for the front-end crates' tests (for the coordinator)

What: docs/v2/crates.md removes `test-fixture` at integration. `crates/blaze-items/src/tests/`
builds its database from fixture text with `test_fixture::WithFixture` (as hir-ty's tests do),
and its `TestDB` uses `hir::impl_hir_database!`. Keep `test-fixture` as a dev-dependency of the
front-end crates, or give its replacement the same `WithFixture` methods (`with_single_file`,
`with_files`, `test_crate`). Why: without it no test of item hashing, facts or the narrowed
hir-ty queries builds.

## items: codegen and link must name callees by identity, not by `InstanceKey` (for `codegen`, `link`)

What: a function's machine code is keyed by its code hash, which names each callee by its
interface hash. So the code must not name a callee by anything a callee's body edit changes:
relocations to a callee name the callee's identity (its `ItemKey` and instance arguments), and
the linker resolves them. Instance arguments must include the impls selected for them, or two
structurally equal types of different impls share code. Why: otherwise a caller's blob, reused
because its code hash is unchanged, calls a stale callee (docs/v2/items.md 6.1).

## items: positions go to the span side table, never into blobs keyed by code hash (for `codegen`, `format`)

What: `form` keeps a body's MIR spans out of its digest (`Sub::spans`, a separate hash). So
code keyed by the code hash must not bake in positions (panic locations, `#[track_caller]`
locations, debug info): they are filled at link time from `blaze_format::SpanRef`s, keyed by
`Sub::spans`. `line!()`/`column!()`/`file!()` expand to literals today (hir-expand, db): for
them to stop being code, their expansion must be a `SpanRef` the linker fills; until then they
are literals, in the code hash, which is correct but recompiles a function when a line above it
moves. Why: a position change otherwise either recompiles (slow) or, if left out of the key,
runs with stale positions (wrong).

## items: inference records the field names and `Resolves` it reads (for `db`)

What: `Fact::Field` and `Fact::Resolves` stand for a field by position and a name in a scope.
Body inference reads field names (`s.x`, struct literals and patterns) and resolves paths;
for those facts to be what a body depends on, the db's dependency recording must record
`Fact::Field { ty, field }` and `Fact::Resolves { scope, name }` at those reads
(`blaze_items::Fact` constructors in `crates/blaze-items/src/facts.rs`) rather than the whole
`VariantFields` or `ItemScope` read through salsa. Why: docs/v2/items.md 6.2; otherwise adding
a field or an item reruns every body that read the struct or the module.

## items-hirty: name index on `ImplItems` and `TraitItems` (for `db`, `crates/hir-def/src/nameres/assoc.rs`)

What: keep, with `items: Box<[(Name, AssocItemId)]>`, an index from name to the first function
or constant (and the first type alias) of that name, built once in `ImplItems::of` /
`TraitItems::query_with_diagnostics`. Order and first-match semantics unchanged.

Why: performance (principles.md: everything is ours). Method probes now ask
`impl_value_item` / `trait_value_item` per (impl or trait, name), and so do the solver's and
path lowering's lookups once narrowed. Each of those scans the item list linearly, for every
trait in scope at every method call. With an index, each is one hash probe. If profiling shows
the per-(container, name) memos themselves cost too much on cold builds, the same index lets
the db record the per-name fact without a memo per name. No behavior change.
