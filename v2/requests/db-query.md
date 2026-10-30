# Requests from db-query (crates/blaze-query)

## 1. blaze-format: a `Writer` over a caller's buffer (owner of `crates/blaze-format`)

- What: `Writer::with_buffer(schema: SchemaHash, bytes: Vec<u8>) -> Writer` (clears `bytes`,
  writes the header as `Writer::new` does, keeps the capacity), and `Writer::finish_into(self,
  root) -> Vec<u8>` returning the buffer so the caller can reuse it.
- Where: `crates/blaze-format/src/lib.rs`, beside `Writer::new` (line ~606).
- Why: every query run calls `blaze_query::write_value`, which does `Writer::new`, a fresh
  4 KiB `Vec` for each run. blaze-query then copies the bytes once into its thread's value
  segment. With a reusable buffer, a run allocates nothing for its value once the buffer has
  grown (docs/v2/plan/goals/principles.md, "preallocate and reuse buffers"). Miss path only;
  hits already allocate nothing. Once it exists, blaze-query keeps one buffer per thread in its
  thread-local run stacks (`lib.rs`, `Stacks`).

> applied (format is DONE): `Writer::with_buffer(schema, bytes)` in `crates/blaze-format/src/lib.rs`, which clears `bytes`, keeps its capacity and writes the header; `Writer::new` is it over a fresh 4 KiB `Vec`. No `finish_into`: `Writer::finish` already returns the buffer by value, so hand that back to `with_buffer`. Test `a_reused_buffer_writes_the_same_bytes_in_its_own_allocation` (`tests.rs`).

## 2. Interface note, no action needed: `Query::fingerprint` takes the value's bytes

`Query::fingerprint(value)` is now `Query::fingerprint(value, bytes)`, with `bytes` the value as
written, and its default hashes those bytes instead of writing the value a second time. No
crate in the tree overrides it (`rg 'fn fingerprint' crates` finds no `impl Query` that does),
so no caller changes. If `docs/v2/plan/reference/crates.md` ever lists the trait's methods, it
should show the new signature.

> applied: nothing to change; `docs/v2/plan/reference/crates.md` lists no `Query` methods.
