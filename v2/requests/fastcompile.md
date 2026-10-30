# Requests of lane `fastcompile`

The lane's crates and boundaries: `docs/v2/plan/lanes/fastcompile/notes.md`. Children of the
lane append their own requests below, each under a heading naming the child.

## 1. coordinator: workspace dependencies for the lane's new crates (root `Cargo.toml`)

What: in `[workspace.dependencies]`, beside `blaze-link`:

```toml
blaze-global = { path = "crates/blaze-global" }
blaze-hashcons = { path = "crates/blaze-hashcons" }
blaze-history = { path = "crates/blaze-history" }
blaze-inproc = { path = "crates/blaze-inproc" }
blaze-intent = { path = "crates/blaze-intent" }
blaze-lazy = { path = "crates/blaze-lazy" }
blaze-premap = { path = "crates/blaze-premap" }
blaze-stencil = { path = "crates/blaze-stencil" }
hir-ty-share = { path = "crates/hir-ty-share", version = "0.0.0" }
```

Why: the twelve changes of `docs/v2/plan/design/compile-time.md` live in these crates
(`docs/v2/plan/process/building.md` section 18); they depend on each other (`blaze-lazy` and
`blaze-premap` on `blaze-inproc`, `blaze-lazy` on `blaze-stencil`) and other lanes' hooks name
them, all with `workspace = true`. Members come from the `crates/*` glob; each crate arrives
with its `Cargo.toml` and `src/lib.rs` together. `blaze-testsel` is added here only if
`fastcompile-ident-select` creates it.

> partly applied: `blaze-global`, `blaze-hashcons`, `blaze-history`, `blaze-inproc`, `blaze-lazy` in the root `[workspace.dependencies]` (each has its `Cargo.toml` and `src/lib.rs`). `blaze-intent`, `blaze-premap` (no `src/lib.rs` yet), `blaze-stencil`, `hir-ty-share` go in when each crate is whole.

## fastcompile-ident-select 1: coordinator: workspace dependency `blaze-testsel` (root `Cargo.toml`)

What: in `[workspace.dependencies]`, beside the lane's other crates:

```toml
blaze-testsel = { path = "crates/blaze-testsel" }
```

Why: exact test selection (idea 3 of `docs/v2/plan/design/compile-time.md`) needs pieces that
have no home in `blaze-usercode`: keys over the functions a test executed (by identity and
bound code, `docs/v2/plan/lanes/fastcompile/select.md`), the record of executed functions, and
the cached mark. `blaze-cargo` depends on it to select and record tests (request
`fastcompile-ident-select 7`). It depends on `blaze-usercode`, `blaze-cas`, `blaze-core`,
`blaze-ids` and `blaze-api`, none of which depend on it.

> applied: `blaze-testsel = { path = "crates/blaze-testsel" }` in the root `[workspace.dependencies]`.

## fastcompile-ident-hashcons 1: db: `Query::fingerprint` is handed the bytes already written (`crates/blaze-query/src/lib.rs`)

What, in `crates/blaze-query/src/lib.rs`:

- trait `Query`, item `fn fingerprint`: take the value's bytes too, and hash those by default:

  ```rust
  /// The value's fingerprint: by default the hash of `bytes`, the value as the database keeps
  /// it. A hash-consed value (`blaze_hashcons::Interned`, or a value of them) returns the
  /// fingerprint it was given when interned, which hashes nothing.
  fn fingerprint(value: &Self::Value, bytes: &[u8]) -> Fingerprint {
      let _ = value;
      value_fingerprint(bytes)
  }
  ```

- fn `run_query`: `let fingerprint = Q::fingerprint(&value, &bytes);` (the `bytes` it has just
  written with `write_value`).

Why: early cutoff in blaze-query already compares fingerprints, never values
(`Database::verify` compares each `Edge::saw` with the fingerprint now; `MemoData::same` and
`value_of`'s determinism check compare `MemoData::fingerprint`). The whole-value cost is in
making the fingerprint: the default today serializes the value a second time
(`write_value(value)` inside `Query::fingerprint`, after `run_query`'s own `write_value`) and
BLAKE3s all of it, on every execution. Handed the bytes, a query that doesn't opt in writes its
value once instead of twice, so not opting in costs less than today. A query whose value is
hash-consed (`type Value = blaze_hashcons::Interned<'static, T>`, which persists as `T`'s bytes)
overrides it with `value.fingerprint()`: 128 bits computed once when the value was interned, and
for a value made of interned parts, from its parts' content IDs (Merkle), so a rerun that makes
an equal value hashes nothing for cutoff. No implementor overrides `fingerprint` today
(`rg 'fn fingerprint\(value' crates`), so only the default and `run_query` change.

> db (18:45): accepted. It is assigned to db-query, which owns `crates/blaze-query` and does it
> first, with a test that an overriding query keeps early cutoff; it marks this applied when
> done. One condition for implementors: an overridden fingerprint must change whenever the
> value's bytes change. The image, cold packs and `why` key a value by these bytes, and
> verification trusts the fingerprint alone.

## fastcompile-ident-hashcons 2: db: salsa needs no change for hash-consed outputs (`vendor/salsa/src/function/backdate.rs`)

What: nothing to apply; recorded so no one adds a second path. Salsa backdates in
`IngredientImpl::backdate_if_appropriate` (`vendor/salsa/src/function/backdate.rs`): with the
old value present it calls `Configuration::values_equal`, which
`vendor/salsa-macros/src/tracked_fn.rs` (`Macro::try_fn`, the `eq` quote) generates as
`old_value == new_value`; with the old value evicted it compares `value_fingerprint`s, made by
`Configuration::fingerprint` / `Fingerprint::of_value` over the output's `Hash`.

Why: a tracked function whose output is `blaze_hashcons::Interned<'static, T>` (or a struct of
them) opts in by its output type alone. Its `==` is one pointer compare instead of an `Eq` walk,
and its `Hash` writes its 128-bit content fingerprint, so `Fingerprint::of_value` hashes 16
bytes, the same in every run. It is `'static` (the interner is leaked with the database, as
`crates/intern`'s are), so `SalsaValue` accepts it directly. Functions that don't opt in are
unchanged.

> db (18:45): confirmed against db-salsa's part 2. While the old value is kept, salsa
> backdates by `==`; once it is evicted, by the value fingerprint taken at eviction over its
> `Hash`. `can_backdate` (durability, cycle heads) gates both. Nothing to change in salsa.
