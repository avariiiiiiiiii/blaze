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

## 3. Root `Cargo.toml`: `blaze-mem` as a workspace dependency (owner of the root manifest)

- What: `blaze-mem = { path = "crates/blaze-mem" }` under `[workspace.dependencies]`, beside
  `blaze-reclaim` (line ~140).
- Why: blaze-query's columns (`table::Rows`: memo rows, value rows, every map's entries) find a
  row through a dependent load of its chunk's base. Over `blaze_mem::Region` (reserved once,
  committed as it grows, never moved) a row's address is base + index × size, which takes two
  dependent L1 loads off every warm hit: from ≈ 1.6× to ≈ 1.2× the hot hit's bound
  (`docs/v2/plan/lanes/db/speed-of-light.md`, "blaze-query", "Warm hit"). Once it exists,
  blaze-query adds `blaze-mem.workspace = true` and moves `Rows` onto regions tagged
  `Tag::Query`.

> applied by requests: `blaze-mem = { path = \"crates/blaze-mem\" }` beside `blaze-reclaim`; new to the lock.

## 4. blaze-graph: run a batch of closures as plane tasks and wait for them, from outside a task (owner of `crates/blaze-graph`)

- What: something like `blaze_graph::scope(|spawn| { spawn(job); ... })`, which runs each job as
  a plane task and returns once all of them have finished. The caller is a thread that is not in
  a task (the daemon calling `Database::collect`).
- Why: `collect`'s marking is one thread walking every reachable memo, about 1.1 s at
  nushell's scale against 6 ms on all cores
  (`docs/v2/plan/lanes/db/speed-of-light.md`, "blaze-query", "Collection"). The marks are
  already a bitset by memo ID, so parallel marking is a `fetch_or` on the shared words, with
  the roots split across jobs. The database must not start threads of its own, so it needs the
  plane's.

> applied (core is DONE; nothing new needed): `blaze_graph::pool::Pool::scope(|scope| { scope.submit(None, job, done); .. })` (`pool.rs:417`) is it: jobs that borrow what outlives the call, run on the pool's workers, the call returning once every one has run (a job's panic goes to its `done`). From a thread that is not a worker it parks; on a worker it pumps. The database takes the daemon's `&Pool` (as `ide-db`'s `run_on_pool` does) and starts no threads.
