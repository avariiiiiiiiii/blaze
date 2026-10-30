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
