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
