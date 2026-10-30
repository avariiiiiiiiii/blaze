# Requests from lane items

## items-facts: `InferenceResult::defined_anon_consts` (`crates/hir-ty/src/infer.rs`)

What: a public accessor for the anonymous consts a body's inference defined (its inline
`const { .. }` blocks, array lengths and const arguments written in the body):

```rust
impl<'db> InferenceResult<'db> {
    /// The anonymous consts of the body this inferred, including those inside them.
    pub fn defined_anon_consts(&self) -> &[AnonConstId<'db>] {
        &self.defined_anon_consts
    }
}
```

Where: `crates/hir-ty/src/infer.rs`, on `InferenceResult` (the field `defined_anon_consts` is
private, line ~862).

Why: `Fact::Captures(key)` resolves its key to `Resolved::Closure { store, expr, kind }` and
has to rebuild the closure's `InternedClosureId`, whose owner is the `InferBodyId` that
inferred it: the body, or the anonymous const the closure is inside (`infer_anon_const_query`
infers those apart and `merge_anon_consts` merges them). Guessing the body as owner would name
a closure id inference never made, so its captures would be wrong. For signatures and fields
`AnonConstId::all_from_signature` and `field_types_with_diagnostics(..).defined_anon_consts()`
already list them; for bodies only the inference result has the list.
`crates/blaze-items/src/facts.rs` (`closure_owner`) calls
`InferenceResult::of(db, body).defined_anon_consts()` as written above; it does not compile
until this lands. Read-only, no behavior change.
