# Requests from lane usercode-concurrency

Findings 1–10 of `v2/requests/usercode.md`, "From concurrency: lock audit (2026-10-29)",
against `docs/v2/plan/design/concurrency.md`. The code in `crates/blaze-expand/`,
`crates/blaze-macro-wasm/` and `crates/proc-macro-api/` is written against exactly the items
below. None of it has been built: the lane may not build.

## 1. cargo (`crates/blaze-cargo/src/serve/`): publish the expansions' puts

Where: the end of each plane task that can expand a macro (anything that asks hir of a crate),
on the worker that ran it; and the store's owner.

Why: an expansion no longer writes the store. Finding 1 and 4: `keep` took
`Expansions::store.write()` on the expansion's worker, stalling every other expansion's lookup
and the store's owner. A kept expansion is now a `blaze_expand::Put` in a buffer of the thread
that ran it, and nothing is kept until the owner publishes it. **Until this lands no
expansion is kept**: every build expands every macro, found only what earlier builds kept.

```rust
// On the worker, as the task ends (cheap: moves a Vec out of a thread-local).
let puts: Vec<blaze_expand::Put> = blaze_expand::take_puts();
// Send `puts` to the store's owner with the task's completion; on the owner:
let published = blaze_expand::publish(&mut store, puts); // store: &mut blaze_cas::Store
// published.tickets: Vec<blaze_cas::Ticket>, as `index::keep` gave them: settle them as
// any other put's.
// published.refused: Vec<String>, each naming the expansion and why (a second, different
// value under one key: the key misses an input or the macro is not deterministic; or a
// corrupt store). `publish` also records them so `blaze_expand::server_failed()` fails
// the build: fail the task's build with them, as with `server_failed`.
```

`take_puts` also returns the puts of threads that ended with puts not taken, so calling it
once more after the workers are joined loses nothing. A put taken twice is published once
(`blaze_cas::Store::put` of the same bytes is `Ok`), and a different value under a kept key is
the same `Mismatch` error it was before.

> forwarded to cargo: `crates/blaze-cargo/src/serve/` is lane cargo's.

## 2. cargo: more warm proc-macro servers on demand

Where: where `ProcMacroClient::warm` is called (usercode-macros 2).

Why: finding 9. A warm client held one connection per server and expanded on it under a lock,
so a macro whose expansion asked the database for something that expanded another macro of
the same library deadlocked, and concurrent expansions of one library waited on each other.
Each expansion now claims a connection for itself; when every one is claimed, the client opens
another.

```rust
impl proc_macro_api::warm::Servers for TheDaemonsPool {
    /// Another warm server like the client's others (the same `Server`, `WarmKey` and
    /// sandbox), ready and bridged as usercode-macros 2 says: `warm::Warm { link, traced }`.
    fn connect(&self) -> std::io::Result<proc_macro_api::warm::Warm>;
}
ProcMacroClient::warm(program, servers, Arc::new(the_pool) as Arc<dyn warm::Servers>, environment)
```

`Servers: Send + Sync`; `connect` is called from the thread of the expansion that found every
connection claimed, so it may block until the pool has one ready. An error from it fails that
expansion, naming it.

> forwarded to cargo: lane cargo's.

## 3. store (`crates/blaze-cas`): `blaze_cas::Reader`

Why: findings 1 and 4. Expansions still look up through `Expansions::store`, an
`Arc<RwLock<blaze_cas::Store>>`, taking its read guard for each `index::find`; nothing writes it
from an expansion any more (item 1), so the guard is never contended by an expansion, but it
is still a shared lock on the read path and still waits for the owner's writes.

```rust
pub struct Reader { … }                 // Send + Sync, callable from any thread
impl Reader {
    pub fn get(&self, key: &Key) -> Result<Option<Entry<'_>>, Corrupt>;
}
```

`blaze_usercode::index::find` would then take a `&Reader` (a request for usercode's
`blaze-usercode`, which I'll make when `Reader` exists), and `Expansions::store` becomes the
`Reader` plus the owner's handle.

> forwarded to concurrency: `crates/blaze-cas` is lane concurrency's now (`crates.md`).

## 4. Withdrawn: usercode-macros 6, the store's write path

The part of usercode-macros 6 that asks the store to "batch writers" is done here: expansions
put through `take_puts`/`publish` (item 1). The read view stands, as item 3.

> applied: noted; withdrawn.

## 5. core-plane: park a task that waits on another's work

Two waits remain blocking a worker, as no task can park on the plane yet:

- `blaze-macro-wasm`: a module that keeps state between calls runs one expansion at a time
  per `(database, crate)`, in order; a second waits on a `Condvar` until the first gives the
  session back. No lock is held while one runs.
- `proc-macro-api`: an expansion waits for its server's reply on the connection it claimed
  (and its sub-requests are answered on the same thread).

When the plane can park a task on an event and resume it, both become parks.

## 6. safety: the audit's findings in my crates

Applied, comments and splits only (usercode.md "From safety-audit": blaze-macro-wasm,
blaze-expand's `native/proc_macro`; proc-macro-api's and proc-macro-srv's were already done).
Two stay open because fixing them changes behavior, and each now has a SAFETY comment that
names the unchecked assumption and points at the finding:

- MAY BE UNSOUND 4 (`blaze-macro-wasm/src/lib.rs`, `Wasm::compile`): the `.cwasm` beside the
  module is loaded as machine code unchecked. Proposed fix, mine to make when allowed: load
  machine code only from the store (`get_lasting`, checksummed), or hash the file and compare
  it with the store's entry under `key` before `deserialize_file`.
- MAY BE UNSOUND 5 (`blaze-macro-wasm/guest/proc_macro.rs`, `__blaze::expand1`/`expand2`):
  make them `unsafe fn` with a `# Safety` on the input pointers, and the exports that
  `macro_list.rs` generates (lane cargo's / codegen's `crates/blaze-engine/src/macro_list.rs`)
  call them in an `unsafe {}` block. The two changes must land together.

> forwarded to coordinator: lane core is DONE; parking a task on an event is new plane work for a lane the coordinator names.
