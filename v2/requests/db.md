# Requests of lane db

## 1. `crates/blaze-query/src/lib.rs`: `Query::Key` serializable and `'static`

```rust
type Key: Clone + Send + Sync + Serialize + serde::de::DeserializeOwned + 'static;
```

Why: a result is verified by walking its edges, and an edge names a dependency only by its
`QueryKey` (kind and content ID). When that dependency has no valid memo in the view, the
database must run it again to see whether its fingerprint changed (early cutoff), so it needs
the dependency's key itself, not its hash: after a daemon start from the image, after the
settle swap, and for any dependency the asking query never names again. The database keeps
each key's bytes (deterministic CBOR, `blaze-protocol`'s) beside its memos and in the image.
`'static` because a typed key is passed through the database's kind table as `&dyn Any`.
Every key in the plan (item keys, facts, paths, instance keys) already derives serde. The
lane's code in `crates/blaze-query/` is written against this bound: it does not build without
it.

> applied: by db (see 4).

## 2. vfs (or the coordinator's daemon glue): implement `blaze_query::Tree` over the VFS

`blaze_query::Tree` (new, `crates/blaze-query/src/tree.rs`) is how the database reads a view's
files: `file(root, path) -> Option<(ContentId, Arc<[u8]>)>` and
`list(root, path) -> Option<Vec<(String, bool)>>`, both pure functions of a root, called from
many worker threads. The VFS's nodes are content-addressed and immutable, so a handle that
shares them (not `&mut Vfs`) can answer both. Wire it in `Database::new(Config { tree, .. })`.

> applied: `impl Tree for blaze_vfs::Reader` is in `crates/blaze-query/src/tree.rs`; wiring it into the daemon's `Database::new` goes with 3.

## 3. Coordinator, daemon wiring

- Make the daemon's database with `Database::new(Config { tree, dir, tags })` (or, at start,
  `Database::from_image_with(config, mapping)` of `<dir>/image`; on `DbError::Image` start from
  `Database::new` and rebuild from the store's results, as `docs/v2/plan/lanes/format.md` says).
- Hand it every completion whose tag it `owns`, through `Database::complete`.
- After a cold build: `write_image`, then, once `complete` has mapped it, `settle(&mut effects)`
  swaps to the mapped image and drops the cold arenas (#1299). `reclaim` frees retired buffers
  whenever the daemon holds the database alone.
- Call `collect(kept, effects)` between requests with the roots to keep (the last build of each
  head), which also moves cold values to disk.

> forwarded to cargo: the daemon (`blaze serve`, `crates/blaze-cargo/`) is lane cargo's (cargo's request 4).

---
Routed or applied by the coordinator, 16:54.

## 4. Status of request 1 (db, 17:40)

`crates/blaze-query/` is lane db's crate (`docs/v2/plan/reference/crates.md`), so db applied
request 1 itself: `Query::Key: Clone + Send + Sync + Serialize + DeserializeOwned + 'static`. No
crate outside blaze-query implements `Query` yet; `blaze-units`' planned queries (`Plan` with key
`()`, `StdUnits` with `StdKey`) must derive serde on their keys.

> applied: by db.

## 5. blaze-query: `Cancelled` is the plane's, and its single flight is the plane's (db, 17:40)

`blaze_query::Cancelled` is now `pub use blaze_graph::Cancelled` (same unit struct, same name), and
the database's single flight is `blaze_graph::pump::Flights`, so a query's wait pumps and is in
the plane's one wait graph (#1301). Callers that wrote `blaze_query::Cancelled` are unchanged
(`blaze-units`, `blaze-usercode`); none imports both names. Nothing to apply.

> applied: note only.

## 6. Daemon (coordinator): salsa's task depth and `run_all`

What: at daemon start, before any query, call `salsa::set_task_depth(blaze_graph::pump::depth)`
(db-salsa is adding it: `~/restart-2026-10-28/briefs/v2/db-salsa-sync.md`). Implement
`SourceDatabase::run_all` on the daemon's database as one `blaze_graph::pool::Pool::scope` batch
on the shared pool, each job calling `task(self, i)` through `&self`, as db-expand's request says.
Where: the daemon's database (`crates/ide-db/src/lib.rs` `RootDatabase` until it is replaced) and
the daemon's start.
Why: salsa's query stack becomes per task (thread and pump depth). A job the pool pumps under a
waiting query on the same thread must start with an empty stack, or its reads become edges of the
query below it; and if it asks a key that query holds it must be cancelled and reposted, not taken
for a query cycle. Salsa can't depend on blaze-graph, so the daemon hands it the depth.

> applied: `RootDatabase::run_all` in `crates/ide-db/src/lib.rs` runs the batch in one `Pool::scope` on the pool given by `RootDatabase::set_pool` (see `v2/requests/db-expand.md`); it needs `salsa::Storage` to be `Sync` to compile.
> forwarded to cargo: `salsa::set_task_depth(blaze_graph::pump::depth)` at the daemon's start and `RootDatabase::set_pool` with the daemon's pool are the daemon's (`set_task_depth` is not in `vendor/salsa` yet: db-salsa's).

## 7. Docs (coordinator)

`docs/v2/plan/lanes/db.md` moved to `docs/v2/plan/lanes/db/query-database.md` (lane notes go in the lane's
folder); the one link to it (`crates/blaze-query/src/lib.rs`) is updated. Index it wherever
`python3 tools/docs.py check` wants lane notes listed.

> forwarded to coordinator: indexing lane notes is `docs/v2/`'s, not among the paths `requests` applies.

## From concurrency: lock audit (2026-10-29)

Audited against `docs/v2/plan/design/concurrency.md` by `concurrency-audit-front` (read-only).
Each item: where, what the lock guards, which rule it breaks, what to do instead.

1. **`crates/blaze-query/src/table.rs:24` (`Shards`, `RwLock` per shard; `SHARDS = 64` at :19).**
   Every memo lookup goes through it: `Cx::get` → `prepare` → `learn_key` takes a read lock
   (`lib.rs:1000`), `resolve` → `state.results.get` takes another (`lib.rs:1107`), `Cx::file` /
   `dir` / `config` → `input_value` → `state.inputs.get` (`lib.rs:1038`). An `RwLock` read is an
   atomic write to the shard's counter, and `Shards::get` (:52) returns `V: Clone`, so a view hit
   also does an `Arc<MemoData>` refcount increment (a second shared write), and `slot()` (`lib.rs:984`)
   clones a whole `Slot` (a `Vec<Arc<_>>` plus an `Arc<[u8]>`, so it allocates) on each verify.
   Breaks "a read writes no shared memory" and "nothing allocated on a hit". The count is fixed
   at 64, which breaks "shard count ≥ 4× cores, chosen at startup". Instead: the key's input is a hash
   already, so make each table an append-only open-addressed table indexed by `input` bits. Slots
   hold an `AtomicPtr<MemoData>` (or `<InputValue>`) published with one release store and read
   with acquire, and the table grows by adding a level, never rehashing, as `blaze-ids::Sharded`
   does. Readers get `&'db MemoData`, not an `Arc` clone: memos dropped from a slot already go to
   `retired`, freed only by `&mut Database` (`reclaim`), which is quiescent-state reclamation at
   the task boundary. `Slot.memos` becomes an immutable published list (swap in a new one on
   `add_memo`, retire the old one). Writers lock a shard chosen from a startup count
   (`available_parallelism() * 4`, next power of two) only to claim the slot.
2. **`crates/blaze-query/src/table.rs:190` (`MemoData.value: Mutex<Value>`).** Every value read
   takes it: `value_of` → `memo_bytes` → `MemoData::bytes` (:204, from `lib.rs:1255`), and
   `stored()` / `memory()` do too. A hot memo read by every worker (a crate's def map) puts all
   of them on one mutex. Breaks "a read writes no shared memory" and "values are immutable once
   published". Instead, hold an `AtomicPtr<Location>` to an immutable boxed `Value`. `evict`, `to_disk` and
   `refill` build a new `Location` and `compare_exchange` it in, then push the old one to
   `retired` (the lifetime argument at `lib.rs:1256` already relies on retiring). Readers do one
   acquire load.
3. **`vendor/salsa/src/function/eviction/lru.rs:21` (`Lru.set: Mutex<FxLinkedHashSet<Id>>`,
   locked by `insert` at :27).** `IngredientImpl::fetch` calls `self.eviction.record_use(id)` on
   **every hit** (`function/fetch.rs:34`), so each ingredient with `lru = N` serializes all
   workers on one mutex per lookup. That covers `body` / `body_with_source_map`
   (`hir-def/src/expr_store/body.rs:94,144`), macro expansion (`hir-expand/src/lib.rs:503,1775`),
   attrs (`hir-def/src/attrs.rs:1316,1328,1377,1401`), `editioned_file_id`
   (`base-db/src/editioned_file_id.rs:22`), and the `lru = 0` queries (`infer`, the const evals,
   whose type is still `Lru`), plus MIR bodies once `set_lru_capacity` runs
   (`hir-ty-mir/src/mir/lower.rs:4886`). Breaks "bookkeeping on a read stays on the worker" and
   "no `Mutex` on a memo lookup". Instead, push the `Id` to a per-worker buffer (in `ZalsaLocal`,
   or the plane task's state) and merge it into the LRU in `for_each_evicted`, which already runs
   under `&mut`. Or keep a clock bit per memo row, set with one relaxed `fetch_or` and swept at
   the new revision.
4. **`vendor/salsa/src/interned.rs:437` (`IngredientImpl.shards: [CachePadded<Mutex<IngredientShard>>]`).**
   `intern_id` locks the shard **before** looking, so every intern of a value that already exists
   takes a mutex. That covers every hir-def ID (`impl_intern_key!` at `base-db/src/lib.rs:60`,
   `FunctionLoc` and the rest), `MacroCallId` (`hir-expand/src/lib.rs:1575`) and the hir-ty IDs
   (`hir-ty/src/db.rs:474-653`). The hit path also writes the row unconditionally:
   `value.durability.store(durability)` (:492) on every hit inside a query, plus the LRU move
   (:468) for reusable values. Breaks "a read writes no shared memory" (interner lookups). The
   shard count (`max_parallelism() * 4`, :1013) is fine. Instead, probe lock-free first, the way
   `crates/intern/src/arena.rs` does (SwissTable control words published with release, `ValueKey`
   pointers are stable), and lock the shard only to look again and insert. On a hit, raise
   durability with `fetch_max` only when it actually increases. For `revisions = usize::MAX`
   (every front-end interned struct here) skip the LRU and `last_interned_at` entirely. For
   GC'd ones, make the per-revision bump a CAS on `last_interned_at` and keep LRU recency in a
   per-worker buffer. The locked path in `maybe_changed_after` (:1133) takes the shard lock once
   per value per revision just to store `last_interned_at`; a CAS from the old revision does the
   same without it.
5. **`vendor/salsa/src/function/sync.rs:75` (`SyncTable` shards, `Mutex<HashTable<SyncState>>`).**
   This is the write path (claiming a query to run), and the lock is held only to claim, which is
   right. But the count is `max_parallelism().next_power_of_two().max(2)`, about 1× cores, below
   the 4× rule. Use `(max_parallelism() * 4).next_power_of_two()` as `interned.rs:1014` does.
   Also, when the key is running, `try_claim` keeps the shard guard and passes it into
   `Runtime::block`, which then takes the global `dependency_graph` lock (`runtime.rs:328`) under
   it (see 6).
6. **`vendor/salsa/src/runtime.rs:41` (`dependency_graph: Mutex<DependencyGraph>`, one per
   database) and `runtime/dependency_graph.rs:89` (`cvar.wait`).** A worker that finds its key
   claimed by another thread takes this one global lock while still holding the sync shard
   guard, then **parks the OS thread** on a condvar until the other finishes. Every completing
   query that had waiters takes the same global lock again (`unblock_queries_blocked_on`, :386).
   Also, `Running::block_on` runs `zalsa.event(WillBlockOn)` (:146) with both the
   dependency-graph guard and the sync-shard guard held, so an event callback that reaches the
   database would deadlock. This breaks "no thread waits on another's work" (single flight parks
   the task, not the worker) and "no guard lives across a callback that can reach a query".
   Instead, route salsa's "key is running elsewhere" case through the plane's single flight
   (`blaze_graph::pump::Flights::get_or_run`, as `blaze-query/src/lib.rs:1139` does), so the
   waiting task parks and the worker pumps other work. Cycle detection moves onto the plane's
   wait graph, the one global lock goes away, and the event is sent after every guard is
   dropped.
7. **`crates/hir-ty/src/next_solver/interner/tls_cache.rs:96` (a `Mutex<Slot>` per slot of the
   process-wide solver cache) and `:99` (`claim: Mutex<()>`, one for the whole table).** Every
   global-cache lookup and insert of the trait solver (`rustc_type_ir` `search_graph/mod.rs:1211,
   1230,1446` → `with_global_cache` → `borrow_assume_valid` → `Table::with` :126) locks a slot,
   and holds that lock while it runs the solver's closure. I checked: the closure only touches
   the `GlobalCache` (`mk_tracked` / `get_tracked` are plain), so the lock isn't held across a
   query today, but nothing enforces that. Each new interner's first solve takes the one global
   `claim` lock (:178), and `Solved::new` (:30) and `clock.fetch_add` (:190) are process-wide
   counters written per query. This breaks "no `Mutex` on a read path" and "writers shard by
   hash" (a single `claim`). Instead: a `Solved` cache belongs to one query, which is one plane
   task, so keep it in that task's own state (it moves with the task when the task changes
   threads) and drop the shared table, which removes every lock. If it must stay shared, claim a
   slot with a CAS on `keys[at]` (no `claim` mutex) plus an owner word CAS'd by the task, and
   hand out `Solved` numbers from per-worker blocks.
8. **`crates/base-db/src/lib.rs:84-115` (`Files`: a `DashMap` for each table, and `views:
   RwLock`).** These are read from inside queries through `SourceDatabase`
   (`load-cargo/src/v2/db.rs:64-139`), and each `DashMap` shard is an `RwLock`:
   - `file_text` (:127), `source_root` (:429) and `file_source_root` (:473) take a shard read
     lock on every call from a query (parse, source-root lookups). Instead: `FileId` and
     `SourceRootId` are dense `u32`s, and every writer already takes `&mut dyn SourceDatabase`,
     so use chunked append-only arrays indexed by ID (`[OnceLock<Box<[OnceLock<FileText>]>>]`, as
     `blaze-ids` does) read with an acquire load, with no lock.
   - `included_file` (:331-346), reached from a query via `include_bytes!` / `include_str!`
     (`hir-expand/src/builtin/fn_macro.rs:1012`), holds the `included` shard's **write** guard
     (`entry(..).or_insert_with`) across a blocking `std::fs::read` (`read_included` :364, which
     also takes the `views` read lock in `on_disk` :320) and across
     `IncludedFile::builder(..).new(db)`, a call into the database. That breaks "no guard across
     a call into the database" and "no blocking call on a worker". Instead: look the file up; if
     it's missing, read it and make the input with no guard held, then `or_insert` the result and
     drop the loser. Better, read the file as a blaze-query `Cx::file` input, so it is neither a
     blocking read nor untracked. Make `views` an immutable list behind an `AtomicPtr`, swapped
     by `set_view` and reclaimed at quiescence.
   - `env_is_read` (:386), `env_is_unset` (:396) and `include_source` (:370), called from queries
     (`fn_macro.rs:874,1044,1067`, `hir-def/src/nameres/mod_resolution.rs:104`,
     `nameres/frozen.rs:2032`), each do a shard write lock and a `String` / `PathBuf` allocation
     per read: bookkeeping on a read in shared memory. Instead, record them in the task's own
     buffer, merged when the task ends, or as edges of the query (`Cx::read`) so the env var or
     path is an input.
   Status: done by speed-hir-basedb. `Files` (now `crates/base-db/src/files.rs`) reads
   `file_text`/`has_file_text`/`source_root`/`file_source_root` from dense paged columns (two
   acquire loads, no lock); `included_file` reads the file and makes its input with no guard
   held, then claims a place in a lock-free-read table; `views` is an immutable list in an
   append-only log published by one release store; `env_is_read`/`env_is_unset`/
   `include_source` look up lock-free and allocate only the first time a key is noted. Setter
   state is behind one `Mutex<Writer>` no query takes. `CratesMap` and `next_file` as judged
   below (one lock per `set_in_db`; a plain field of the writer).
9. **No enforcement yet.** `concurrency.md` ("Waiting") says `clippy.toml`'s `disallowed-types`
   bans `std::sync::Mutex` and `RwLock` in `blaze-query`, `blaze-ids`, `intern` and
   `blaze-graph`, but the root `.clippy.toml` has no such entry, and `blaze-query` uses both
   (items 1 and 2). Once 1 and 2 land, ask the coordinator for the ban (clippy reads one
   `clippy.toml` per crate, so a crate-local one has to repeat the root's entries).

Judged off the hot path, nothing to do: `blaze-query` `Database.views` (`lib.rs:589`, one read
per request root in `Database::get`; `Cx` carries its `Arc<ViewState>`), `io` / `retired`
(`lib.rs:592-593`: effect completions, GC, eviction, close), `MemTree.roots` (`tree.rs:38`:
tests and what-ifs; daemons read through `blaze_vfs::Reader`); salsa `Table.non_full_pages`
(`table.rs:31`: once per 128-slot page per thread), `Coordinate.clones` (`storage.rs:100`: handle
clone and drop; tasks share the database by reference, `base-db/src/lib.rs:882`),
`RevisionQueue.lock` (`interned.rs:1322`: once per revision), `sync.rs`'s `shuttle` shim and
salsa's examples and tests; `base-db` `CratesMap` (`input.rs:814`) and `next_file` (:98),
written while loading with `&mut db`; `hir/src/check.rs:104` (one slot per task, written once and
never contended); `hir-def/src/nameres/frozen.rs:113` (`Check`/`Verify` modes only, once per
crate); and the test databases (`hir-def/src/test_db.rs`, `hir/src/tests/db.rs`).

> forwarded to db: every finding is in lane db's own crates (`blaze-query`, `vendor/salsa`, the front end); lane db is still running.

## speed-hir-store: hir onto the HIR column views (from speed-hir-store-tools)

From speed-hir-store-tools, 2026-10-29. 30 sites in 5 files of `crates/hir/src/`. The rule is
"## format-hir: expressions and patterns become columns" in `v2/requests/format.md` (contract:
`~/restart-2026-10-28/briefs/v2/format-hir-columns-api.md`). These land together with the
format-hir change. No site builds an `Expr`/`Pat`, uses `ExpressionStoreBuilder`, stores, hashes
or compares a view, and no borrow changes: every store here is a `&ExpressionStore`/`&Body`, so a
view borrows the store, not `self`.

**Depends on `crates/hir-ide/` (coordinator):** `source_analyzer.rs` calls
`hir_ide::diagnostics::{record_literal_missing_fields, record_pattern_missing_fields}`
(`crates/hir-ide/src/diagnostics/expr.rs:647`, `:690`), which take `expr: &Expr` / `pat: &Pat`.
The edits below pass `body.expr(expr_id)` / `body.pat(pat_id)`, so those two parameters become
`expr: Expr<'_>` / `pat: Pat<'_>` (rule 3) in the same change; the request is filed in
`v2/requests/speed.md` under "## speed-hir-store: coordinator-owned consumers".

Files that import the HIR types and need no change: `display.rs` (only `TypeRef` indexing),
`from_id.rs`, `has_source.rs`, `term_search.rs`, `term_search/` (their `Expr` is
`crate::term_search::Expr`, not HIR's). Imports stay as they are in every file.

> forwarded to db: `crates/hir/` is lane db's. Its two calls into `hir-ide` now take the views: `record_literal_missing_fields(db, infer, id, body.expr(id))`, `record_pattern_missing_fields(.., body.pat(id))` (speed.md, applied).

### crates/hir/src/lib.rs  (8 sites)
- ~L2003  `&body[body.params[self.idx - body.self_param.is_some() as usize].user_written]`  ->  `body.pat(body.params[self.idx - body.self_param.is_some() as usize].user_written)`
- ~L2008, ~L2026 (both in `Param::as_local`, the only two in the file)  `binding_id: *id,`  ->  `binding_id: id,`
- ~L2020  `if let Expr::Closure { args, .. } = &store[c.expr]`  ->  `if let Expr::Closure { args, .. } = store.expr(c.expr)`
- ~L2021  `&& let Pat::Bind { id, .. } = &store[args[self.idx]]`  ->  `&& let Pat::Bind { id, .. } = store.pat(args[self.idx])`
- ~L4404  `ExprOrPatId::ExprId(expr) => matches!(store[expr], Expr::Ref { .. }),`  ->  `ExprOrPatId::ExprId(expr) => matches!(store.expr(expr), Expr::Ref { .. }),`
- ~L4406  `ExprOrPatId::PatId(pat) => match store[pat] {`  ->  `ExprOrPatId::PatId(pat) => match store.pat(pat) {`
- ~L5948  `match &body[self.expr] {`  ->  `match body.expr(self.expr) {` (the arm `hir_def::hir::Expr::InlineAsm(e) => e.operands.get(self.index)?.0.clone()` is unchanged: `e` is `&InlineAsm`)

### crates/hir/src/diagnostics.rs  (3 sites)
- ~L1409  `Err(_) => match &body[expr] {`  ->  `Err(_) => match body.expr(expr) {`
- ~L1411  `match source_map.expr_syntax(*inner) {`  ->  `match source_map.expr_syntax(inner) {`
- ~L1414  `.format_args_implicit_capture(*inner)?`  ->  `.format_args_implicit_capture(inner)?`
- ~L1425 `body.exprs().find(|&(it, _)| {` stays: the item is `(ExprId, Expr<'_>)` and `&(it, _)` copies the id out of the reference `find` passes.

### crates/hir/src/semantics.rs  (5 sites)
- ~L1681  `match &body[expr_id] {`  ->  `match body.expr(expr_id) {` (in `collect_returns`)
- ~L2440  `&& let Expr::Block { unsafe_: Unsafe::Yes, .. } = body[expr_id]`  ->  `&& let Expr::Block { unsafe_: Unsafe::Yes, .. } = body.expr(expr_id)`
- ~L2527  `if let Expr::Path(path) = &store[expr_id] {`  ->  `if let Expr::Path(path) = store.expr(expr_id) {` (`path` is `&Path` borrowed from `store`, so it still outlives the arm)
- ~L2554  `if let Expr::Assignment { target, .. } = store[expr_id] {`  ->  `if let Expr::Assignment { target, .. } = store.expr(expr_id) {`
- ~L2941  `match &self.body[expr] {`  ->  `match self.body.expr(expr) {` (`self.body: &'a ExpressionStore`, so `path` borrows the store, not `self`, and the `&mut self` calls in the arm still compile)

### crates/hir/src/semantics/source_to_def.rs  (2 sites)
- ~L402  `if let crate::Pat::Bind { id, .. } = store[pat_id] {`  ->  `if let crate::Pat::Bind { id, .. } = store.pat(pat_id) {`
- ~L440  `let (Expr::Break { label, .. } | Expr::Continue { label }) = store[break_or_continue]`  ->  `let (Expr::Break { label, .. } | Expr::Continue { label }) = store.expr(break_or_continue)`

### crates/hir/src/source_analyzer.rs  (12 sites)
- ~L437  `if let Pat::Bind { id, .. } = self.store()?[pat_id.as_pat()?] { Some(id) } else { None }`  ->  `if let Pat::Bind { id, .. } = self.store()?.pat(pat_id.as_pat()?) { Some(id) } else { None }`
- ~L525  `&& let Expr::Block { tail: Some(tail), .. } = store[expr_id]`  ->  `&& let Expr::Block { tail: Some(tail), .. } = store.expr(expr_id)`
- ~L707  `if let Expr::Field { expr: object_expr, name: _ } = body[field_expr] {`  ->  `if let Expr::Field { expr: object_expr, name: _ } = body.expr(field_expr) {`
- ~L1051  `ExprOrPatId::ExprId(idx) => match &store[idx] {`  ->  `ExprOrPatId::ExprId(idx) => match store.expr(idx) {`
- ~L1055  `ExprOrPatId::PatId(idx) => match &store[idx] {`  ->  `ExprOrPatId::PatId(idx) => match store.pat(idx) {` (both arms' `path` is `&'db Path` from `store: &'db ExpressionStore`, as before)
- ~L1519  `record_literal_missing_fields(db, infer, expr_id, &body[expr_id])?;`  ->  `record_literal_missing_fields(db, infer, expr_id, body.expr(expr_id))?;` (needs the hir-ide signature change above)
- ~L1535  `record_literal_matched_fields(db, infer, expr_id, &body[expr_id])?;`  ->  `record_literal_matched_fields(db, infer, expr_id, body.expr(expr_id))?;`
- ~L1553  `record_pattern_missing_fields(db, infer, pat_id, &body[pat_id])?;`  ->  `record_pattern_missing_fields(db, infer, pat_id, body.pat(pat_id))?;` (needs the hir-ide signature change above)
- ~L1570  `record_pattern_matched_fields(db, infer, pat_id, &body[pat_id])?;`  ->  `record_pattern_matched_fields(db, infer, pat_id, body.pat(pat_id))?;`
- ~L2180 (in `fn record_literal_matched_fields`)  `    expr: &Expr,`  ->  `    expr: Expr<'_>,` (the body's `Expr::RecordLit { fields, spread, .. } => (fields, spread)` is unchanged: `fields: &[RecordLitField]`, `spread: RecordSpread` by value, unused)
- ~L2212 (in `fn record_pattern_matched_fields`)  `    pat: &Pat,`  ->  `    pat: Pat<'_>,`
- ~L2215  `Pat::Record { path: _, args, ellipsis } => (args, *ellipsis),`  ->  `Pat::Record { path: _, args, ellipsis } => (args, ellipsis),`

> applied by db-ty-cols-hir: all 30 sites as written (`semantics.rs` has 5, `source_analyzer.rs` 12, `lib.rs` 8, `diagnostics.rs` 3, `source_to_def.rs` 2), nothing done differently; `hir-ide`'s two functions already took the views. 0 sites found beyond the list (every remaining index in `crates/hir/src/` is a `BindingId`, `LabelId` or `PathId`).

## From speed-hir-nameres: `UnconfiguredCode` keeps no `CfgOptions` (2026-10-29)

`hir_def::nameres::diagnostics::DefDiagnosticKind::UnconfiguredCode` lost its `opts: CfgOptions`
field (a clone of the crate's whole cfg set per cfg'd-out item; the options are always the
crate's). In `crates/hir/src/diagnostics.rs:868-877` replace
```rust
DefDiagnosticKind::UnconfiguredCode { ast_id, cfg, opts } => {
```
with
```rust
DefDiagnosticKind::UnconfiguredCode { ast_id, cfg } => {
```
and `opts: opts.clone(),` with `opts: self.krate.cfg_options(self.db).clone(),` (`self.krate` is
the `base_db::Crate` whose def map's diagnostics these are). The clone now happens only when an
`InactiveCode` diagnostic is emitted, not once per disabled item at collection.

> forwarded to db: `hir-def`'s nameres and `hir` are lane db's.

> applied by db-ty-cols-hir: checked, already in `crates/hir/src/diagnostics.rs` (`UnconfiguredCode { ast_id, cfg }`, `opts: self.krate.cfg_options(self.db).clone()`). `collect_enum`'s `InactiveCode` keeps `diag.opts.clone()`: `InactiveEnumVariantCode` in `hir-def/src/signatures.rs` still has `opts`.

## speed-hir-store: hir onto the HIR column views, type refs (from speed-hir-store-tools)

From speed-hir-store-tools, 2026-10-29. The type-ref half of the block "## speed-hir-store: hir
onto the HIR column views" above: apply both in the same edit of each file. `TypeRef` becomes a
view too (`store.type_ref(t)`, API in the addendum of
`~/restart-2026-10-28/briefs/v2/speed-hir-store-consumers.md`; `Index<TypeRefId>` goes,
`Index<PathId>` and `Index<LifetimeRefId>` stay). 10 sites in 4 files. Every store is a
`&ExpressionStore` from a signature, so no borrow changes. `TypeBound` stays owned, so
`&TypeBound::Path(path, _)` patterns and `store[path]` (a `PathId`) stay. `pub use
hir_def::type_ref::TypeRef` in `lib.rs` now re-exports the view; only `blaze-diagnostics`
imports it and reads it through `hir_def` already.

> forwarded to db: `crates/hir/` is lane db's; `hir-def` does not have the type-ref view yet (`Index<TypeRefId>` is still there).

### crates/hir/src/lib.rs  (1 site)
- ~L2053  `.map(|&param| match &func_data.store[param] {`  ->  `.map(|&param| match func_data.store.type_ref(param) {` (`TypeRef::Reference(ref_) => match ref_.mutability` is unchanged)

### crates/hir/src/display.rs  (6 sites)
- ~L249 and ~L273 (both in the fn display of a function's return type, the only two in the file)  `match &data.store[ret_type] {`  ->  `match data.store.type_ref(ret_type) {` (`TypeRef::ImplTrait(bounds) => match &bounds[0]` and `TypeRef::Tuple(tup) if tup.is_empty()` are unchanged)
- ~L327  `match &data.store[param] {`  ->  `match data.store.type_ref(param) {`
- ~L329  `TypeRef::Reference(ref_) if matches!(&data.store[ref_.ty], TypeRef::Path(p) if p.is_self_type()) =>`  ->  `TypeRef::Reference(ref_) if matches!(data.store.type_ref(ref_.ty), TypeRef::Path(p) if p.is_self_type()) =>`
- ~L794  `if  matches!(store[*target],`  ->  `if  matches!(store.type_ref(*target),`
- ~L811  `matches!(store[target],`  ->  `matches!(store.type_ref(target),`

### crates/hir/src/diagnostics.rs  (2 sites)
- ~L1663  `if let TypeRef::ImplTrait(_) = self.store[ty] {`  ->  `if let TypeRef::ImplTrait(_) = self.store.type_ref(ty) {`
- ~L1680  `let TypeRef::ImplTrait(bounds) = &store[it] else { return None };`  ->  `let TypeRef::ImplTrait(bounds) = store.type_ref(it) else { return None };` (`&bounds[..]` and `store[*path]` below are unchanged)

### crates/hir/src/semantics.rs  (1 site)
- ~L1729  `let path = match &analyze.store()?.types[ty] {`  ->  `let path = match analyze.store()?.type_ref(ty) {` (this read the store's `types` arena field directly, which the columns remove; `path` is `&'db Path`, as before)

> applied by db-ty-cols-hir: all 10 sites as written (`hir-def` now has `ExpressionStore::type_ref`), nothing done differently. 0 sites found beyond the list.

## From speed-runtime-vfs: `crates/blaze-query/src/tree.rs`, two copies the reader no longer needs (2026-10-29)

`blaze_vfs::Reader::file` now returns its `Arc<[u8]>` owned and `Reader::list` an owned
`std::vec::IntoIter<(String, bool)>`, so the impl's `.clone()` and `.to_owned()` copy again
what is already owned (it compiles as it is; this is only the waste). The exact edit, in
`impl Tree for blaze_vfs::Reader`:

```rust
    fn file(&self, root: ContentId, path: &str) -> Option<(ContentId, Mode, Arc<[u8]>)> {
        blaze_vfs::Reader::file(self, root, path)
    }

    fn list(&self, root: ContentId, path: &str) -> Option<Vec<(String, bool)>> {
        blaze_vfs::Reader::list(self, root, path).map(Iterator::collect)
    }
```

and in its doc, replace "The reader lends a file's bytes; this trait gives an `Arc`, so `file`
takes one count" with "The trait gives an `Arc`, so `file` takes one count" (lent bytes are
`Reader::pin()`'s `Pinned::file`, for when the database holds a view's bytes as lent:
v2/requests/vfs.md item 8).

> forwarded to db: `crates/blaze-query/` is lane db's.

## From speed-link-edge-wire: `Edge`'s, `Changed`'s and `Why`'s fields in their keys' order (2026-10-29)

`crates/blaze-api` sends `blaze_query::Edge` (in `Response::Trace`), `Why` and `Changed` (in
`Response::Why`). Its CBOR encoder now writes a map as it comes and reorders it only when its
keys come out of order. These types are not declared in key order (shorter name first, then
bytewise), so each one is reordered on every frame. Declare their fields in key order; the
bytes, the API's `SCHEMA` and every caller (named fields) are unchanged:

- `Edge { saw, read }`
- `Changed { key, now, was }`
- `Why { chain, input, query }`

`QueryKey { kind, input }` is in key order already.

> forwarded to db: `crates/blaze-query/` is lane db's.

## From speed-salsa: salsa's memo and edge layout change (S5, 2026-10-29; note, nothing to do)

`vendor/salsa` (speed-salsa now) changes how a front-end memo and its edges are stored, to meet
data.md's "a memo 24 plus 4 per edge" and #734. Nothing in `crates/` changes: every public API
stays. What changes, so it lines up when the front end moves onto `blaze-query`:

- An edge is one `u32` word: a dense **node** (31 bits, rustc's `DepNodeIndex`) and an output bit.
  Nodes come from a per-database node table (`DatabaseKeyIndex` to node, lock-free SwissTable;
  12 bytes per key ever read). blaze-query's `Edge { read: QueryKey, saw }` maps to a node
  mapped through that table plus the fingerprint kept by position in the memo's cold box.
- A memo's edges are a `(start, len)` range of one CSR array per database (pages that never
  move; a task reserves 256 words at a time and publishes its edges with the memo), compacted
  at a new revision once it doubles. No allocation per memo for edges.
- A memo header is 24 bytes: `verified_at`, one word of changed-at plus flags, and the edge
  range or a box of cold data (cycle state, tracked struct IDs, accumulated values, fingerprints).
- Deep verification runs without a claim while it only reads; only a memo that must run claims.

Design and the mapping onto `MemoData`/`Edge`: `docs/v2/plan/lanes/db/speed-salsa.md`, "S5".

> applied: noted; it asks for no change (no public API moves).

## speed-hir-store: hir-ty onto the HIR column views (from speed-hir-store-ty)

134 sites in 14 files (expressions, patterns and type refs). The rule: `v2/requests/format.md`, "## format-hir: expressions and patterns become columns"; for type refs (`store[t]` -> `store.type_ref(t)`), the addendum of `~/restart-2026-10-28/briefs/v2/speed-hir-store-consumers.md`. Both have landed in `crates/hir-def/src/{hir.rs,hir/type_ref.rs,expr_store.rs}`: `Index<ExprId>`, `Index<PatId>` and `Index<TypeRefId>` are gone, `Index<BindingId|LabelId|LifetimeRefId|PathId>` stay. `InferenceContext::store` and `TyLoweringContext::store` are `&'db ExpressionStore`, so every view borrows the store, not `self`: no borrow conflict changes anywhere below. `mir.rs` is speed-types' file now (its brief); its one edit is here.

> forwarded to db: every file is db's (none of items' hir-ty files).

> applied by db-ty-cols-ty: `db.rs`, `lower.rs`, `lower/path.rs`, `generics.rs`, `consteval.rs`, `next_solver/interner.rs` and `effects.rs`, exactly as listed. 0 sites found beyond the list in db-ty-cols-ty's files (every `store[..]` left indexes a `LifetimeRefId` or `PathId`); none in items' files (`method_resolution*`, `traits.rs`, `specialization.rs`, `opaques.rs`, `layout*`, `drop.rs`).

### crates/hir-ty/src/db.rs  (4 sites)
- ~L547, ~L581, ~L615 (all three occurrences, in `InternedClosureId::new`, `InternedCoroutineId::new`, `InternedCoroutineClosureId::new`)  `let expr = &store[loc.expr];`  ->  `let expr = store.expr(loc.expr);`
- ~L724  `hir_def::expr_store::ExpressionStore::of(db, loc.owner)[loc.expr],`  ->  `hir_def::expr_store::ExpressionStore::of(db, loc.owner).expr(loc.expr),`
- ~L525 `closure_exprs`: `.filter(|(_, expr)| matches!(expr, hir_def::hir::Expr::Closure { .. }))` needs no change (`exprs()` now yields `(ExprId, Expr<'_>)`; the filter closure still gets `&(_, _)` and `matches!` binds through the reference).

### crates/hir-ty/src/lower.rs  (7 sites: 1 pat, 6 type ref)
- ~L966  `let pat_kind = match self.store[pat] {`  ->  `let pat_kind = match self.store.pat(pat) {`
- ~L977  `hir_def::hir::Pat::Or(ref pats)`  ->  `hir_def::hir::Pat::Or(pats)`
  (`pats: &'db [PatId]` borrows `*self.store`, a `&'db ExpressionStore` field, so the recursive `self.lower_pattern_type` (`&mut self`) inside the `.map` still borrow-checks, as before.)
- ~L783 (type ref; `lower_ty_ext`)  `let type_ref = &self.store[type_ref_id];`  ->  `let type_ref = self.store.type_ref(type_ref_id);`
- ~L797, ~L804, ~L813, ~L953 (type ref; arms of that same `match type_ref`, drop the `&`):
  - `&TypeRef::TypeParam(type_param_id) => {`  ->  `TypeRef::TypeParam(type_param_id) => {`
  - `&TypeRef::RawPtr(inner, mutability) => {`  ->  `TypeRef::RawPtr(inner, mutability) => {`
  - `&TypeRef::Slice(inner) => {`  ->  `TypeRef::Slice(inner) => {`
  - `&TypeRef::PatternType(ty, pat) => {`  ->  `TypeRef::PatternType(ty, pat) => {`
  (The other arms need no edit: `Tuple(inner)` is `&[TypeRefId]` with `inner.iter().map(|&tr| ..)`; `Path(path)`, `Array(array)`, `Reference(ref_)` bind `&T` as before; `Fn(fn_)` is now `&FnType`, which `lower_fn_ptr(&mut self, fn_: &FnType)` takes; `DynTrait(bounds)`/`ImplTrait(bounds)` are `&[TypeBound]`, which `lower_dyn_trait`/`lower_impl_trait` take. The view borrows `*self.store`, not `self`, so the `&mut self` calls in the arms borrow-check as before.)
- ~L827 (type ref)  `let inner_ty = match &self.store[ref_.ty] {`  ->  `let inner_ty = match self.store.type_ref(ref_.ty) {`
- ~L1047 (type ref; `lower_ty_only_param`)  `let type_ref = &self.store[type_ref];`  ->  `let type_ref = self.store.type_ref(type_ref);`
- ~L1050 (type ref)  `&TypeRef::TypeParam(idx) => return Some(idx.into()),`  ->  `TypeRef::TypeParam(idx) => return Some(idx.into()),`
- ~L2596 (type ref; supertraits from where clauses)  `let target = &signature.store[*target];`  ->  `let target = signature.store.type_ref(*target);`
  (`target` binds `&TypeRefId` from `pred: &WherePredicate`, so `*target` stays; the arm `TypeRef::TypeParam(param) if param.local_id() == ..` now binds `param: TypeParamId` by value, which `local_id()` takes as well.)
- ~L3975 (type ref; `input_lifetimes`)  `match &store[ty] {`  ->  `match store.type_ref(ty) {`
- ~L3977 (type ref)  `if matches!(&store[it.ty], TypeRef::Path(path) if path.is_self_type()) {`  ->  `if matches!(store.type_ref(it.ty), TypeRef::Path(path) if path.is_self_type()) {`
- ~L3987 (type ref; only the occurrence on the line right after `TypeRef::RawPtr(it, _) | TypeRef::Slice(it) => {`; the identical line at ~L4060 under `GenericArg::Type(it)` binds from `&GenericArg` and keeps its `*`)  `input_lifetimes(db, resolver, store, *it, found, self_references)`  ->  `input_lifetimes(db, resolver, store, it, found, self_references)`
  (`TypeRef::DynTrait(bounds) => for bound in bounds` iterates `&[TypeBound]`, giving `&TypeBound` as before; `TypeBound::Lifetime(it) => .. Some(*it)` keeps its `*`.)

### crates/hir-ty/src/lower/path.rs  (3 sites, type ref)
- ~L837 (`provided_object`)  `let TypeRef::DynTrait(bounds) = &store[type_ref] else { return None };`  ->  `let TypeRef::DynTrait(bounds) = store.type_ref(type_ref) else { return None };`
- ~L1028  `match (&this.ctx.store[type_ref], this.ctx.impl_trait_mode.mode) {`  ->  `match (this.ctx.store.type_ref(type_ref), this.ctx.impl_trait_mode.mode) {`
- ~L1530 (`type_like_const`)  `let type_ref = &store[type_ref];`  ->  `let type_ref = store.type_ref(type_ref);`
  (`TypeRef::Path(path)` binds `&'_ Path` tied to the `store` parameter, matching the return type `Option<TypeLikeConst<'_>>`.)

### crates/hir-ty/src/generics.rs  (1 site, type ref; `Generics::object_lifetime_default`)
- ~L202  `let bounded = match &owner.store[target] {`  ->  `let bounded = match owner.store.type_ref(target) {`
- ~L203  `&TypeRef::TypeParam(it) => it == param,`  ->  `TypeRef::TypeParam(it) => it == param,`
  (`target` binds by value from the `&WherePredicate::TypeBound { .. }` pattern. `match &owner.store[lifetime]` at ~L217 indexes a `LifetimeRefId` and stays.)

### crates/hir-ty/src/consteval.rs  (2 sites, 3 edits; `create_anon_const` and `const_eval_discriminant_variant`)
- ~L347  `let mut expr = &store[expr_id];`  ->  `let mut expr = store.expr(expr_id);`
- ~L354  `expr = &store[*tail];`  ->  `expr = store.expr(tail);`
- ~L414  `if matches!(body[body.root_expr()], Expr::Missing) {`  ->  `if matches!(body.expr(body.root_expr()), Expr::Missing) {`
  (The `match expr` arms `Expr::Literal(literal)` / `Expr::Path(path)` keep `&Literal` / `&Path`; no change.)

### crates/hir-ty/src/mir.rs  (1 site; `is_by_move_coroutine`)
- ~L2167  `} = &store[expr]`  ->  `} = store.expr(expr)`

> applied (speed-types): `mir.rs` 2167. No other expression, pattern or type ref indexing is left in `mir.rs` or `mir/`.

### crates/hir-ty/src/next_solver/interner.rs  (1 site; `CoroutinesVisitor::on_expr`)
- ~L2140  `} = self.store[expr]`  ->  `} = self.store.expr(expr)`
  (`kind @ (...)` binds the `Copy` `ClosureKind` by value, as before.)

### crates/hir-ty/src/effects.rs  (1 site, 3 edits; `Walk::expr`)
- ~L277  `match &self.body[expr] {`  ->  `match self.body.expr(expr) {`
- ~L280  `self.expr(*body);`  ->  `self.expr(body);`
- ~L284  `Expr::Call { callee, .. } => self.call(expr, Some(*callee)),`  ->  `Expr::Call { callee, .. } => self.call(expr, Some(callee)),`

### crates/hir-ty/src/infer/expr.rs  (45 sites)

`self.store` is `InferenceContext::store: &'db ExpressionStore`, so every `self.store.expr(..)` / `self.store.pat(..)` below yields `Expr<'db>` / `Pat<'db>` without borrowing `self`, as the old `&'db Expr` did. No borrow changes.

Pattern views (`fn pat_guaranteed_to_constitute_read_for_never`, `fn contains_explicit_ref_binding`, `fn is_syntactic_place_expr`):
- ~L167  `match &self.store[pat] {`  ->  `match self.store.pat(pat) {`
- ~L210  `if let Pat::Bind { id, .. } = self.store[pat]`  ->  `if let Pat::Bind { id, .. } = self.store.pat(pat)`  (the next line's `self.store[id]` is a `BindingId`: stays)
- ~L222  `match &self.store[expr] {`  ->  `match self.store.expr(expr) {`  (arms `Expr::Path(Path::LangItem(_, _))`, `Expr::Path(Path::Normal(path))`, `Expr::Path(path)` match through the `&'a Path` field unchanged)

`fn infer_expr_inner_here` (the scrutinee becomes a by-value view, so every `&Expr::..` arm loses its `&` and every `*field` of an id/`Copy` field loses its `*`):
- ~L336  `let expr = &self.store[tgt_expr];`  ->  `let expr = self.store.expr(tgt_expr);`
- ~L340, 394, 412, 519, 529, 586, 587, 647: drop the leading `&` of the arm pattern, each unique:
  - `&Expr::If { condition, then_branch, else_branch } => {`  ->  `Expr::If { condition, then_branch, else_branch } => {`
  - `&Expr::Let { pat, expr } => {`  ->  `Expr::Let { pat, expr } => {`
  - `&Expr::Loop { body, label, source } => {`  ->  `Expr::Loop { body, label, source } => {`
  - `&Expr::Continue { label } => {`  ->  `Expr::Continue { label } => {`
  - `&Expr::Break { expr, label } => {`  ->  `Expr::Break { expr, label } => {`
  - `&Expr::Return { expr } => self.infer_expr_return(tgt_expr, expr),`  ->  `Expr::Return { expr } => self.infer_expr_return(tgt_expr, expr),`
  - `&Expr::Become { expr } => self.infer_expr_become(tgt_expr, expr),`  ->  `Expr::Become { expr } => self.infer_expr_become(tgt_expr, expr),`
  - `&Expr::Assignment { target, value } => {`  ->  `Expr::Assignment { target, value } => {`
- ~L404  `self.infer_block(tgt_expr, statements, *tail, *label, expected)`  ->  `self.infer_block(tgt_expr, statements, tail, label, expected)`
- ~L408  `this.infer_expr(*id, expected, ExprIsRead::Yes)`  ->  `this.infer_expr(id, expected, ExprIsRead::Yes)`
- ~L436-442 (`Expr::Closure` arm; `args: &[PatId]` and `arg_types: &[Option<TypeRefId>]` pass to `infer_closure` as before):

  old:
  ```
                  .infer_closure(
                      *body,
                      args,
                      *ret_type,
                      arg_types,
                      *closure_kind,
  ```
  new:
  ```
                  .infer_closure(
                      body,
                      args,
                      ret_type,
                      arg_types,
                      closure_kind,
  ```
- ~L446  `self.infer_call(tgt_expr, *callee, args, expected)`  ->  `self.infer_call(tgt_expr, callee, args, expected)`
- ~L448-453 (`Expr::MethodCall` arm):

  old:
  ```
                  .infer_method_call(
                      tgt_expr,
                      *receiver,
                      args,
                      method_name,
                      generic_args.as_deref(),
  ```
  new:
  ```
                  .infer_method_call(
                      tgt_expr,
                      receiver,
                      args,
                      method_name,
                      generic_args,
  ```
- ~L465-466 (`Expr::Match` arm):

  old:
  ```
                  let input_ty = self.demand_scrutinee_type(
                      *expr,
  ```
  new:
  ```
                  let input_ty = self.demand_scrutinee_type(
                      expr,
  ```
- ~L487  `_ => self.table.next_ty_var((*expr).into()),`  ->  `_ => self.table.next_ty_var(expr.into()),`
- ~L591-592 (`Expr::Yield` arm; `if let Some(expr) = expr` above it now binds `ExprId` by value):

  old:
  ```
                          self.infer_expr_coerce(
                              *expr,
                              &Expectation::has_type(yield_ty),
  ```
  new:
  ```
                          self.infer_expr_coerce(
                              expr,
                              &Expectation::has_type(yield_ty),
  ```
- ~L615  `if let &Some(expr) = expr {`  ->  `if let Some(expr) = expr {`
- ~L621  `self.infer_record_expr(tgt_expr, expected, path, fields, *spread)`  ->  `self.infer_record_expr(tgt_expr, expected, path, fields, spread)`
- ~L623  `self.infer_field_access(tgt_expr, *expr, name, expected)`  ->  `self.infer_field_access(tgt_expr, expr, name, expected)`
- ~L624  `self.infer_await_expr(tgt_expr, *expr)`  ->  `self.infer_await_expr(tgt_expr, expr)`
- ~L626  `let cast_ty = self.make_ty(*type_ref);`  ->  `let cast_ty = self.make_ty(type_ref);`
- ~L628  `self.infer_expr(*expr, &Expectation::Castable(cast_ty), ExprIsRead::Yes);`  ->  `self.infer_expr(expr, &Expectation::Castable(cast_ty), ExprIsRead::Yes);`
- ~L629  `CastCheck::new(tgt_expr, *expr, expr_ty, cast_ty)`  ->  `CastCheck::new(tgt_expr, expr, expr_ty, cast_ty)`
- ~L632-635 (`Expr::Ref` arm):

  old:
  ```
              Expr::Ref { expr, rawness, mutability } => self.infer_ref_expr(
                  *rawness,
                  lower_mutability(*mutability),
                  *expr,
  ```
  new:
  ```
              Expr::Ref { expr, rawness, mutability } => self.infer_ref_expr(
                  rawness,
                  lower_mutability(mutability),
                  expr,
  ```
- ~L639  `self.infer_unop_expr(*op, *expr, expected, tgt_expr)`  ->  `self.infer_unop_expr(op, expr, expected, tgt_expr)`
- ~L642  `self.infer_assign_op_expr(tgt_expr, *op, *lhs, *rhs)`  ->  `self.infer_assign_op_expr(tgt_expr, op, lhs, rhs)`  (`match op` on the by-value `Option<BinaryOp>` binds the inner `op` by value)
- ~L644  `Some(op) => self.infer_binop_expr(tgt_expr, *op, *lhs, *rhs),`  ->  `Some(op) => self.infer_binop_expr(tgt_expr, op, lhs, rhs),`
- ~L653  `let lhs_ty = match &self.store[target] {`  ->  `let lhs_ty = match self.store.pat(target) {`
- ~L655  `&Pat::Expr(expr) => {`  ->  `Pat::Expr(expr) => {`
- ~L687  `let base_t = self.infer_expr_no_expect(*base, ExprIsRead::Yes);`  ->  `let base_t = self.infer_expr_no_expect(base, ExprIsRead::Yes);`
- ~L688  `let idx_t = self.infer_expr_no_expect(*index, ExprIsRead::Yes);`  ->  `let idx_t = self.infer_expr_no_expect(index, ExprIsRead::Yes);`
- ~L690  `let base_t = self.structurally_resolve_type((*base).into(), base_t);`  ->  `let base_t = self.structurally_resolve_type(base.into(), base_t);`
- ~L691  `match self.lookup_indexing(tgt_expr, *base, *index, base_t, idx_t) {`  ->  `match self.lookup_indexing(tgt_expr, base, index, base_t, idx_t) {`
- ~L693-695 (`Expr::Index` arm):

  old:
  ```
                          // two-phase not needed because index_ty is never mutable
                          self.demand_coerce(
                              *index,
  ```
  new:
  ```
                          // two-phase not needed because index_ty is never mutable
                          self.demand_coerce(
                              index,
  ```
- ~L737  `self.infer_array_repeat_expr(*initializer, *repeat, expected, tgt_expr)`  ->  `self.infer_array_repeat_expr(initializer, repeat, expected, tgt_expr)`

`fn check_call_arguments`:
- ~L2160  `let is_closure = if let Expr::Closure { closure_kind, .. } = self.store[*arg] {`  ->  `let is_closure = if let Expr::Closure { closure_kind, .. } = self.store.expr(*arg) {`  (`arg` is `&ExprId` from `provided_args.iter()`: the `*` stays)

Unchanged in `expr.rs`, checked: `Expr::Block { statements, .. }` (now `&[Statement]`, same as `infer_block`'s parameter), `Expr::Match { arms, .. }` (`&[MatchArm]`; `for arm in arms` / `arms.iter()` / `arms.is_empty()` as before), `Expr::Tuple { exprs, .. }` (the `*expr` at ~L728 derefs `exprs.iter()`'s `&ExprId`: stays), `Expr::Array(Array::ElementList { elements })` (`&[ExprId]`), `Expr::Path(p)` / `Expr::Literal(lit)` / `Expr::OffsetOf(offset)` / `Expr::InlineAsm(asm)` / `Expr::Field { name, .. }` / `Expr::RecordLit { path, fields, .. }` (references as before), the `Statement::Let` / `&Statement::Expr` arms in `fn infer_block` (`Statement` is unchanged and still iterated from `&[Statement]`), `tracing::trace!(?expr)` (the view is `Debug`), and the `hir::{..}` import list (every name still used). There are no `TypeRef` index sites in this file (`type_ref` values only go to `self.make_ty`).


### crates/hir-ty/src/infer/closure/analysis.rs  (9 sites)
- ~L212  `match &self.store[expr] {`  ->  `match self.store.expr(expr) {`
- ~L217  `*body,`  ->  `body,`
- ~L218  `*capture_by,`  ->  `capture_by,`
- ~L219  `*closure_kind,`  ->  `closure_kind,`
- ~L292  `= self.store[parent_hir_id]`  ->  `= self.store.expr(parent_hir_id)`
- ~L319  `let Expr::Block { statements, .. } = &self.store[body] else {`  ->  `let Expr::Block { statements, .. } = self.store.expr(body) else {`
- ~L326  `let Pat::Bind { .. } = self.store[pat] else {`  ->  `let Pat::Bind { .. } = self.store.pat(pat) else {`
- ~L330  `let Expr::Path(path) = &self.store[init] else {`  ->  `let Expr::Path(path) = self.store.expr(init) else {`
- ~L555  `let Expr::Block { tail: Some(body), .. } = self.store[body] else {`  ->  `let Expr::Block { tail: Some(body), .. } = self.store.expr(body) else {`

(L1031 `self.store[var_hir_id]` indexes by `BindingId` (`PlaceBase::Upvar { var_id: BindingId }`): stays.)

### crates/hir-ty/src/infer/closure/analysis/expr_use_visitor.rs  (24 sites)
- ~L510, ~L1304 (2x, `walk_expr_here` and `cat_expr_unadjusted`)  `match self.cx.store[expr] {`  ->  `match self.cx.store.expr(expr) {`
- ~L529  `Expr::Call { callee, ref args } => {`  ->  `Expr::Call { callee, args } => {`
- ~L535  `Expr::MethodCall { receiver, ref args, .. } => {`  ->  `Expr::MethodCall { receiver, args, .. } => {`
- ~L541  `Expr::RecordLit { ref fields, spread, .. } => {`  ->  `Expr::RecordLit { fields, spread, .. } => {`
- ~L545  `Expr::Tuple { ref exprs } => {`  ->  `Expr::Tuple { exprs } => {`
- ~L565  `Expr::Match { expr: discr, ref arms } => {`  ->  `Expr::Match { expr: discr, arms } => {`
- ~L575  `Expr::Array(Array::ElementList { elements: ref exprs }) => {`  ->  `Expr::Array(Array::ElementList { elements: exprs }) => {`
- ~L587  `Expr::InlineAsm(ref asm) => {`  ->  `Expr::InlineAsm(asm) => {`
- ~L637  `Expr::Block { ref statements, tail, .. } => {`  ->  `Expr::Block { statements, tail, .. } => {`
- ~L905  `debug!("walk_pat: pat.kind={:?}", this.cx.store[pat]);`  ->  `debug!("walk_pat: pat.kind={:?}", this.cx.store.pat(pat));`
- ~L913  `match this.cx.store[pat] {`  ->  `match this.cx.store.pat(pat) {`
- ~L950  `Pat::Path(ref path) => {`  ->  `Pat::Path(path) => {`
- ~L995  `Pat::Slice { prefix: ref lhs, slice: wild, suffix: ref rhs } => {`  ->  `Pat::Slice { prefix: lhs, slice: wild, suffix: rhs } => {`
- ~L997  `if matches!((&**lhs, wild, &**rhs), (&[], Some(_), &[]))`  ->  `if matches!((lhs, wild, rhs), (&[], Some(_), &[]))`
- ~L1201  `} else if let Pat::Ref { pat: subpat, .. } = self.cx.store[pat]`  ->  `} else if let Pat::Ref { pat: subpat, .. } = self.cx.store.pat(pat)`
- ~L1217, ~L1575 (2x, `pat_ty_unadjusted` and `cat_pattern`)  `match self.cx.store[pat] {`  ->  `match self.cx.store.pat(pat) {`
- ~L1344  `Expr::Path(ref path) => {`  ->  `Expr::Path(path) => {`
- ~L1576  `Pat::Tuple { args: ref subpats, ellipsis: dots_pos } => {`  ->  `Pat::Tuple { args: subpats, ellipsis: dots_pos } => {`
- ~L1594  `Pat::TupleStruct { args: ref subpats, ellipsis: dots_pos, .. } => {`  ->  `Pat::TupleStruct { args: subpats, ellipsis: dots_pos, .. } => {`
- ~L1613  `Pat::Record { args: ref field_pats, .. } => {`  ->  `Pat::Record { args: field_pats, .. } => {`
- ~L1636  `Pat::Or(ref pats) => {`  ->  `Pat::Or(pats) => {`
- ~L1659  `Pat::Slice { prefix: ref before, slice, suffix: ref after } => {`  ->  `Pat::Slice { prefix: before, slice, suffix: after } => {`


### crates/hir-ty/src/infer/pat.rs  (11 sites)
- ~L306  `let pat = &self.store[pat_id];`  ->  `let pat = self.store.pat(pat_id);`
- ~L403  `match self.store[pat] {`  ->  `match self.store.pat(pat) {`
- ~L470  `Pat::TupleStruct { args: ref subpats, ellipsis: ddpos, .. } => match opt_path_res`  ->  `Pat::TupleStruct { args: subpats, ellipsis: ddpos, .. } => match opt_path_res`
- ~L484  `Pat::Record { args: ref fields, ellipsis: has_rest_pat, .. } => {`  ->  `Pat::Record { args: fields, ellipsis: has_rest_pat, .. } => {`
- ~L511  `Pat::Or(ref pats) => {`  ->  `Pat::Or(pats) => {`
- ~L517  `Pat::Tuple { args: ref elements, ellipsis: ddpos } => {`  ->  `Pat::Tuple { args: elements, ellipsis: ddpos } => {`
- ~L530  `Pat::Slice { prefix: ref before, slice, suffix: ref after } => {`  ->  `Pat::Slice { prefix: before, slice, suffix: after } => {`
- ~L611  `        pat: &Pat,`  ->  `        pat: Pat<'_>,`  (the parameter of `fn calc_adjust_mode`)
- ~L638  `&Pat::Lit(expr) => {`  ->  `Pat::Lit(expr) => {`
- ~L752  `let literal = match &self.store[expr] {`  ->  `let literal = match self.store.expr(expr) {`
- ~L968  `if let Pat::Bind { .. } = self.store[inner]`  ->  `if let Pat::Bind { .. } = self.store.pat(inner)`

### crates/hir-ty/src/infer/mutability.rs  (24 sites)
- ~L63  `match &self.store[tgt_expr] {`  ->  `match self.store.expr(tgt_expr) {`
- ~L88  `&Expr::If { condition, then_branch, else_branch } => {`  ->  `Expr::If { condition, then_branch, else_branch } => {`
- ~L96  `self.infer_mut_expr(*id, Mutability::Not);`  ->  `self.infer_mut_expr(id, Mutability::Not);`
- ~L98  `Expr::Let { pat, expr } => self.infer_mut_expr(*expr, self.pat_bound_mutability(*pat)),`  ->  `Expr::Let { pat, expr } => self.infer_mut_expr(expr, self.pat_bound_mutability(pat)),`
- ~L117  `self.infer_mut_expr(*tail, Mutability::Not);`  ->  `self.infer_mut_expr(tail, Mutability::Not);`
- ~L122  `args.iter().copied().chain(Some(*it))`  ->  `args.iter().copied().chain(Some(it))`
- ~L126  `self.infer_mut_expr(*expr, m);`  ->  `self.infer_mut_expr(expr, m);`
- ~L138  `if let &Some(expr) = expr {`  ->  `if let Some(expr) = expr {`
- ~L143 and ~L171 (2 sites, the line after `Expr::Become { expr } => {` and the line after `| Expr::Cast { expr, type_ref: _ } => {`): `self.infer_mut_expr(*expr, Mutability::Not);`  ->  `self.infer_mut_expr(expr, Mutability::Not);`. NOT ~L111 (in `Statement::Expr { expr, has_semi: _ }`, a `&Statement` field): that one stays.
- ~L148  `if let RecordSpread::Expr(expr) = *spread {`  ->  `if let RecordSpread::Expr(expr) = spread {`
- ~L151  `&Expr::Index { base, index } => {`  ->  `Expr::Index { base, index } => {`
- ~L160  `self.convert_place_op_to_mutable(PlaceOp::Deref, tgt_expr, *expr, None);`  ->  `self.convert_place_op_to_mutable(PlaceOp::Deref, tgt_expr, expr, None);`
- ~L162, ~L165, ~L175 (3 sites, all in `Expr` arms; replace all in file): `self.infer_mut_expr(*expr, mutability);`  ->  `self.infer_mut_expr(expr, mutability);`
- ~L174  `let mutability = lower_mutability(*mutability);`  ->  `let mutability = lower_mutability(mutability);`
- ~L178  `self.infer_mut_expr(*lhs, Mutability::Mut);`  ->  `self.infer_mut_expr(lhs, Mutability::Mut);`
- ~L179, ~L191 (2 sites, replace all in file): `self.infer_mut_expr(*rhs, Mutability::Not);`  ->  `self.infer_mut_expr(rhs, Mutability::Not);`
- ~L181  `&Expr::Assignment { target, value } => {`  ->  `Expr::Assignment { target, value } => {`
- ~L182  `self.store.walk_pats(target, &mut |pat| match self.store[pat] {`  ->  `self.store.walk_pats(target, &mut |pat| match self.store.pat(pat) {`
- ~L190  `self.infer_mut_expr(*lhs, Mutability::Not);`  ->  `self.infer_mut_expr(lhs, Mutability::Not);`
- ~L194  `self.infer_mut_expr(*body, Mutability::Not);`  ->  `self.infer_mut_expr(body, Mutability::Not);`

### crates/hir-ty/src/infer.rs  (1 site)
- ~L2419  `while let Expr::Block { tail: Some(tail), label: None, .. } = self.store[expr] {`  ->  `while let Expr::Block { tail: Some(tail), label: None, .. } = self.store.expr(expr) {`

> applied by db-ty-cols-infer: every edit of the `infer.rs`, `infer/expr.rs`, `infer/closure/analysis.rs`, `infer/closure/analysis/expr_use_visitor.rs`, `infer/pat.rs` and `infer/mutability.rs` subsections, as written. One difference: `analysis.rs` ~L292 now wraps after `=` (it ran to 102 columns). 0 sites beyond the list: the sweep of `infer.rs` and `infer/` finds only `BindingId` indexes left (`expr.rs` ~L211, `pat.rs` ~L890, `mutability.rs` ~L228, `analysis.rs` ~L1031), and no other `infer/` file matches on `Expr`/`Pat` or takes `&Expr`/`&Pat`.


Files that import the HIR types but need no change: `infer/{op,place_op,coerce,cast,callee,diagnostics,autoderef,fallback,columns,opaques,path,unify}.rs`, `infer/closure.rs`; lane items' files (`method_resolution.rs`, `method_resolution/`, `traits.rs`, `specialization.rs`, `opaques.rs`, `layout.rs`, `layout/`, `drop.rs`) have no site.

Status: open

## speed-hir-store: hir-ty-borrowck onto the HIR column views (from speed-hir-store-ty)

109 sites in 6 files (97 expression/pattern, 12 type ref, all type-ref ones in `regions.rs`). The rule: `v2/requests/format.md`, "## format-hir: expressions and patterns become columns"; type refs per the addendum of `~/restart-2026-10-28/briefs/v2/speed-hir-store-consumers.md` (landed). `Ctx::store` is `&'a ExpressionStore`, so views borrow the store, not `self`: no borrow conflict changes. Two edits are not mechanical: `loans.rs` ~L439 `once(&receiver)` (now `ExprId` beside `args.iter()`'s `&ExprId`), and `regions.rs` ~L2026 deletes `let args = args.clone();` (was a `Box<[PatId]>` clone per tuple pattern; now a slice).

> forwarded to db.

### crates/hir-ty-borrowck/src/regions.rs  (52 sites: 40 expr/pat, 12 type ref)
- ~L1823, L1857, L1958 (3 occurrences, replace all)  `= &self.ctx.store[closure]`  ->  `= self.ctx.store.expr(closure)`
- ~L1904  `let local = match self.ctx.store[pat] {`  ->  `let local = match self.ctx.store.pat(pat) {`
- ~L1921  `if !matches!(self.ctx.store[pat], Pat::Bind { subpat: None, .. }) {`  ->  `if !matches!(self.ctx.store.pat(pat), Pat::Bind { subpat: None, .. }) {`
- ~L1924  `if !matches!(self.ctx.store[pat], Pat::Bind { .. })`  ->  `if !matches!(self.ctx.store.pat(pat), Pat::Bind { .. })`
- ~L1972  `.chain([(super::return_slot(), *ret_type)])`  ->  `.chain([(super::return_slot(), ret_type)])`
- ~L2000, L4174 (2 occurrences, replace all)  `match &self.ctx.store[pat] {`  ->  `match self.ctx.store.pat(pat) {`
- ~L2004  `&& let Some(&local) = self.body.binding_locals.get(*id)`  ->  `&& let Some(&local) = self.body.binding_locals.get(id)`
- ~L2018  `if let Some(subpat) = *subpat {`  ->  `if let Some(subpat) = subpat {`
- ~L2026  `let args = args.clone();` (the whole line)  ->  (delete the line)
- ~L2034  `self.annotated_bindings(*pat, inner, at, false);`  ->  `self.annotated_bindings(pat, inner, at, false);`
- ~L2093  `Some(&up) if matches!(store[up], hir_def::hir::Expr::Closure { .. }) => {`  ->  `Some(&up) if matches!(store.expr(up), hir_def::hir::Expr::Closure { .. }) => {`
- ~L2154  `let spelled = |pat: &Pat| match pat {`  ->  `let spelled = |pat: Pat<'_>| match pat {`
- ~L2166  `&& spelled(&self.ctx.store[*pat])`  ->  `&& spelled(self.ctx.store.pat(*pat))`
- ~L2174  `if spelled(&self.ctx.store[arm.pat]) {`  ->  `if spelled(self.ctx.store.pat(arm.pat)) {`
- ~L2175  `found.push((arm.pat, Some(*expr)));`  ->  `found.push((arm.pat, Some(expr)));`
- ~L2195, L2203 (2 occurrences, replace all)  `Pat::Path(_) = self.ctx.store[pat]`  ->  `Pat::Path(_) = self.ctx.store.pat(pat)`
- ~L2260  `let hir_def::hir::Expr::Path(path) = &self.ctx.store[expr] else { return None };`  ->  `let hir_def::hir::Expr::Path(path) = self.ctx.store.expr(expr) else { return None };`
- ~L2386  `let hir_def::hir::Expr::Path(path) = &self.ctx.store[expr] else { return };`  ->  `let hir_def::hir::Expr::Path(path) = self.ctx.store.expr(expr) else { return };`
- ~L2408  `if let Pat::Bind { id, subpat: None } = self.ctx.store[annotation.pat]`  ->  `if let Pat::Bind { id, subpat: None } = self.ctx.store.pat(annotation.pat)`
- ~L3149  `self.ctx.store[coroutine],`  ->  `self.ctx.store.expr(coroutine),`
- ~L3900, L5363, L5386 (3 occurrences, replace all)  `match self.ctx.store[call] {`  ->  `match self.ctx.store.expr(call) {`
- ~L3928, L4170 (2 occurrences, replace all)  `match &self.ctx.store[expr] {`  ->  `match self.ctx.store.expr(expr) {`
- ~L4068  `&& let hir_def::hir::Expr::Cast { type_ref, .. } = self.ctx.store[expr]`  ->  `&& let hir_def::hir::Expr::Cast { type_ref, .. } = self.ctx.store.expr(expr)`
- ~L4993  `let spelled = |expr| match &store[expr] {`  ->  `let spelled = |expr| match store.expr(expr) {`
- ~L5002  `MirSpan::ExprId(expr) => match store[expr] {`  ->  `MirSpan::ExprId(expr) => match store.expr(expr) {`
- ~L5034  `let names_self = |expr| match &store[expr] {`  ->  `let names_self = |expr| match store.expr(expr) {`
- ~L5045, L5058 (2 occurrences, replace all)  `|| matches!(store[expr],`  ->  `|| matches!(store.expr(expr),`
- ~L5075  `MirSpan::ExprId(expr) => match &store[expr] {`  ->  `MirSpan::ExprId(expr) => match store.expr(expr) {`
- ~L5076  `hir_def::hir::Expr::Call { callee, .. } => Some(*callee),`  ->  `hir_def::hir::Expr::Call { callee, .. } => Some(callee),`
- ~L5083  `&& let hir_def::hir::Expr::Path(path) = &store[callee]`  ->  `&& let hir_def::hir::Expr::Path(path) = store.expr(callee)`
- ~L5616  `&self.ctx.store[loc.expr],`  ->  `self.ctx.store.expr(loc.expr),`
- ~L5915  `self.ctx.store[expr],`  ->  `self.ctx.store.expr(expr),`
- ~L305 (type ref)  `let written = signature.ret_type.and_then(|ret| match &signature.store[ret] {`  ->  `let written = signature.ret_type.and_then(|ret| match signature.store.type_ref(ret) {`
- ~L482, L6678, L6713, L6748 (type ref; 4 occurrences, replace all)  `match &store[ty] {`  ->  `match store.type_ref(ty) {`
- ~L494 (type ref; the line right after `TypeRef::RawPtr(it, _) | TypeRef::Slice(it) | TypeRef::PatternType(it, _) => {`, which is the only `nth_impl_trait(store, *it, index)` alone on its line)  `nth_impl_trait(store, *it, index)`  ->  `nth_impl_trait(store, it, index)`
- ~L5453 (type ref)  `matches!(&written.store[it],`  ->  `matches!(written.store.type_ref(it),`
- ~L6681 (type ref)  `TypeRef::RawPtr(it, _) | TypeRef::Slice(it) => writes_self(store, *it),`  ->  `TypeRef::RawPtr(it, _) | TypeRef::Slice(it) => writes_self(store, it),`
- ~L6717 (type ref)  `TypeRef::RawPtr(it, _) | TypeRef::Slice(it) => writes_elided(store, *it),`  ->  `TypeRef::RawPtr(it, _) | TypeRef::Slice(it) => writes_elided(store, it),`
- ~L6755 (type ref)  `TypeRef::RawPtr(it, _) | TypeRef::Slice(it) => count(*it),`  ->  `TypeRef::RawPtr(it, _) | TypeRef::Slice(it) => count(it),`
- ~L6790 (type ref)  `store.types.iter().any(|(_, it)| match it {`  ->  `store.type_refs().any(|(_, it)| match it {`
- ~L6799 (type ref)  `store.types.iter().any(|(_, it)| {`  ->  `store.type_refs().any(|(_, it)| {`

Lines tagged "(type ref)" are the speed-hir-store addendum (type refs onto columns, `ExpressionStore::type_ref`/`type_refs`); Indexing by `LifetimeRefId` (L375, L402, L412, L6702, L6751, L6774, L6802) and `PathId` (L477, L6722) stays. Sites read and needing no change: L825 and L2085 (`exprs()` mapped to ids), L2102-2128 (`Expr::Path(path)` over the `exprs()` view still binds `&Path`), L2161-2178 apart from the lines above (`statements`/`arms` are slices, `Statement`/`MatchArm` unchanged), L2420-2433 (`lets`).




### crates/hir-ty-borrowck/src/check.rs  (25 sites)
- L846, L917, L1081, L1136, L2029, L2331 (all 6 occurrences, each `expr` an `ExprId`): `self.ctx.store[expr]`  ->  `self.ctx.store.expr(expr)`
- ~L352  `found.insert(*scrutinee);`  ->  `found.insert(scrutinee);`
- ~L366  `found.insert(pat, *scrutinee);`  ->  `found.insert(pat, scrutinee);`
- ~L378  `while let Expr::Block { tail: Some(tail), .. } = ctx.store[at] {`  ->  `while let Expr::Block { tail: Some(tail), .. } = ctx.store.expr(at) {`
- ~L389  `matches!(ctx.store[*pat], Pat::Bind { subpat: None, .. })`  ->  `matches!(ctx.store.pat(*pat), Pat::Bind { subpat: None, .. })`
- ~L390  `matches!(ctx.store[*init], Expr::Block { .. })`  ->  `matches!(ctx.store.expr(*init), Expr::Block { .. })`
- ~L401  `let Pat::Lit(lit) = ctx.store[pat] else { return false };`  ->  `let Pat::Lit(lit) = ctx.store.pat(pat) else { return false };`
- ~L403  `        ctx.store[lit],`  ->  `        ctx.store.expr(lit),`
- ~L415  `match &ctx.store[pat] {`  ->  `match ctx.store.pat(pat) {`
- ~L420  `| Pat::Deref { inner } => irrefutable(ctx, *inner),`  ->  `| Pat::Deref { inner } => irrefutable(ctx, inner),`
- ~L847  `&& let Expr::Block { tail: Some(tail), .. } = self.ctx.store[body] =>`  ->  `&& let Expr::Block { tail: Some(tail), .. } = self.ctx.store.expr(body) =>`
- ~L1555  `self.ctx.store[*expr],`  ->  `self.ctx.store.expr(*expr),`
- ~L1830  `hir_def::hir::Expr::Call { callee: c, .. } if *c == callee)`  ->  `hir_def::hir::Expr::Call { callee: c, .. } if c == callee)`
- ~L2939  `let simple = is_let && matches!(store[pat], Pat::Bind { subpat: None, .. });`  ->  `let simple = is_let && matches!(store.pat(pat), Pat::Bind { subpat: None, .. });`
- ~L2948  `add(arm.pat, *expr, false);`  ->  `add(arm.pat, expr, false);`
- ~L2951  `hir_def::hir::Expr::Let { pat, expr } => add(*pat, *expr, false),`  ->  `hir_def::hir::Expr::Let { pat, expr } => add(pat, expr, false),`
- ~L3101  `if matches!(ctx.store[pat], Pat::Bind { .. }) {`  ->  `if matches!(ctx.store.pat(pat), Pat::Bind { .. }) {`
- ~L3190  `match &ctx.store[expr] {`  ->  `match ctx.store.expr(expr) {`
- ~L3192  `Expr::Let { pat, .. } => pats.push(*pat),`  ->  `Expr::Let { pat, .. } => pats.push(pat),`
- ~L3302  `&& let Expr::Index { index, .. } = ctx.store[expr]`  ->  `&& let Expr::Index { index, .. } = ctx.store.expr(expr)`

Unchanged on purpose: L389/L390/L2955/L3195 keep `*pat`/`*init` (bound from `&Statement`, an owned type in a slice); L1555 keeps `*expr` (bound from `&MirSpan` in a `filter` closure); L403's `Expr::Literal(Literal::Bool(_) | ..)` matches the `&Literal` field by match ergonomics as before.

### crates/hir-ty-borrowck/src/loans.rs  (18 sites)
- L181, L383 (both occurrences): `match &ctx.store[expr] {`  ->  `match ctx.store.expr(expr) {`
- L194, L418 (both occurrences): `ctx.infer.expr_ty(*callee).kind()`  ->  `ctx.infer.expr_ty(callee).kind()`
- ~L122  `match ctx.store[expr] {`  ->  `match ctx.store.expr(expr) {`
- ~L182  `Expr::Const(inner) => stack.push(*inner),`  ->  `Expr::Const(inner) => stack.push(inner),`
- ~L185  `stack.push(*inner);`  ->  `stack.push(inner);`
- ~L191  `if ctx.infer.variant_resolution_for_expr(*callee).is_some()`  ->  `if ctx.infer.variant_resolution_for_expr(callee).is_some()`
- ~L192  `matches!(&ctx.store[*callee], Expr::Path(path) if path.is_self_type())`  ->  `matches!(ctx.store.expr(callee), Expr::Path(path) if path.is_self_type())`
- ~L201  `Expr::Block { tail: Some(tail), .. } => stack.push(*tail),`  ->  `Expr::Block { tail: Some(tail), .. } => stack.push(tail),`
- ~L202  `Expr::Cast { expr, .. } => stack.push(*expr),`  ->  `Expr::Cast { expr, .. } => stack.push(expr),`
- ~L245  `&& let Expr::Ref { expr: inner, mutability, .. } = ctx.store[expr]`  ->  `&& let Expr::Ref { expr: inner, mutability, .. } = ctx.store.expr(expr)`
- ~L247  `matches!(&ctx.store[inner], Expr::Array(Array::ElementList { elements }) if elements.is_empty())`  ->  `matches!(ctx.store.expr(inner), Expr::Array(Array::ElementList { elements }) if elements.is_empty())`
- ~L408  `const_promotable(ctx, body, *initializer, depth + 1)`  ->  `const_promotable(ctx, body, initializer, depth + 1)`
- ~L415  `const_promotable(ctx, body, *expr, depth + 1)`  ->  `const_promotable(ctx, body, expr, depth + 1)`
- ~L439  `let dropped = std::iter::once(receiver)`  ->  `let dropped = std::iter::once(&receiver)`  (receiver is `ExprId` now; the chain with `args.iter()` and `.any(|&it| ..)` needs `&ExprId` items)
- ~L442  `all(std::iter::once(*receiver).chain(args.to_vec()).collect())`  ->  `all(std::iter::once(receiver).chain(args.to_vec()).collect())`
- ~L448  `const_promotable(ctx, body, *tail, depth + 1)`  ->  `const_promotable(ctx, body, tail, depth + 1)`

### crates/hir-ty-borrowck/src/qualifs.rs  (11 sites)
- ~L105  `match &self.store[expr] {`  ->  `match self.store.expr(expr) {`
- ~L121  `&Expr::Array(Array::Repeat { initializer, .. }) =>`  ->  `Expr::Array(Array::Repeat { initializer, .. }) =>`
- ~L127  `if matches!(self.infer.expr_ty(*callee).kind(), TyKind::FnDef(def, _)`  ->  `if matches!(self.infer.expr_ty(callee).kind(), TyKind::FnDef(def, _)`
- ~L132  `&Expr::Ref { expr: inner, .. } =>`  ->  `Expr::Ref { expr: inner, .. } =>`
- ~L133  `&Expr::Cast { expr: inner, .. }`  ->  `Expr::Cast { expr: inner, .. }`
- ~L134  `| &Expr::UnaryOp { expr: inner, op: UnaryOp::Not | UnaryOp::Neg } =>`  ->  `| Expr::UnaryOp { expr: inner, op: UnaryOp::Not | UnaryOp::Neg } =>`
- ~L137  `&Expr::BinaryOp { lhs, rhs, op: Some(op) }`  ->  `Expr::BinaryOp { lhs, rhs, op: Some(op) }`
- ~L146  `self.value(*tail, depth + 1)`  ->  `self.value(tail, depth + 1)`
- ~L183  `if let &Expr::UnaryOp { expr: base, op: UnaryOp::Deref } = &self.store[place]`  ->  `if let Expr::UnaryOp { expr: base, op: UnaryOp::Deref } = self.store.expr(place)`
- ~L202  `match self.store[expr] {`  ->  `match self.store.expr(expr) {`
- ~L212  `let Expr::Path(path) = &self.store[expr] else { return None };`  ->  `let Expr::Path(path) = self.store.expr(expr) else { return None };`

### crates/hir-ty-borrowck/src/lib.rs  (2 sites)
- ~L445  `match &store[expr] {`  ->  `match store.expr(expr) {`
- ~L636  `matches!(self.store[expr], hir_def::hir::Expr::Cast { .. })`  ->  `matches!(self.store.expr(expr), hir_def::hir::Expr::Cast { .. })`

### crates/hir-ty-borrowck/src/liveness.rs  (1 site)
- ~L106  `match ctx.store[*expr] {`  ->  `match ctx.store.expr(*expr) {`  (`*expr` stays: bound from `&Statement`)


Files that need no change: `names.rs` (only `BindingId` indexing), `cfg.rs`, `constraints.rs`, `dataflow.rs`, `gate.rs`, `inference.rs`, `intervals.rs`, `moves.rs`, `polonius.rs`, `relation.rs`, `timings.rs`, `bitset.rs`.

> applied by db-ty-cols-borrowck: every site as listed, nothing done differently; 0 sites found beyond the list (the final `rg` of the crate finds only `BindingId`, `LifetimeRefId` and `PathId` indexes left).

Status: open

## speed-hir-store: hir-ty-wfcheck onto the HIR column views (from speed-hir-store-ty)

1 site in 1 file, a type ref (rule: addendum of `~/restart-2026-10-28/briefs/v2/speed-hir-store-consumers.md`, landed: `Index<TypeRefId>` is gone).

> forwarded to db.

> applied by db-ty-cols-ty: as listed. 0 sites found beyond the list.

### crates/hir-ty-wfcheck/src/lib.rs  (1 site, 2 edits, type ref; the where-clause `position` search)
- ~L576  `let on_target = match (&store[*target], param) {`  ->  `let on_target = match (store.type_ref(*target), param) {`
- ~L577  `(TypeRef::TypeParam(id), Some(param)) => *id == param,`  ->  `(TypeRef::TypeParam(id), Some(param)) => id == param,`
  (`target` binds `&TypeRefId` from `predicate: &WherePredicate`, so `*target` stays. `store[*path]` at ~L583 indexes a `PathId` and stays.)

`outlives.rs` and `persist.rs` read no store: no change.

Status: open

## From safety-audit

From `safety-audit-front` (child of `safety-audit`, lane `safety`). The workspace is about to deny
`unsafe_op_in_unsafe_fn`, `clippy::undocumented_unsafe_blocks` and
`clippy::multiple_unsafe_ops_per_block`. Every file below was BUSY, so these are for you to paste.
Comments, `# Safety` sections, block splits and crate-root attributes only: no behavior change.
A split keeps the order of evaluation and binds only `Copy` values (pointers, integers, the
references the code already made). Line numbers are as of 2026-10-29 18:40.

> forwarded to db: every crate named is db's.

### blaze-query

- `crates/blaze-query/src/table.rs:685` block (`ValueCell::memory`): 2 ops (`Arc::increment_strong_count`, `Arc::from_raw`). Split:
  ```rust
  // SAFETY: `raw` came from `Arc::into_raw` (the cell's count) and stays alive while `task` is, as in `ValueCell::load`; this adds the count the `Arc` below owns. A dead `raw` would be a use after free.
  unsafe { Arc::increment_strong_count(raw) };
  // SAFETY: `raw` came from `Arc::into_raw`, and the line above gave it the count this `Arc` takes over, so the cell's own count is untouched.
  Some(unsafe { Arc::from_raw(raw) })
  ```

### base-db

- `crates/base-db/src/lib.rs:5` crate: no `unsafe` in its compiled code (its `#[salsa::interned(unsafe(no_lifetime))]` at `lib.rs:59` and `editioned_file_id.rs:13` is a salsa option, and salsa's expansions are external-macro spans, which `unsafe_code` skips). Add `#![forbid(unsafe_code)]` after the `#![cfg_attr(feature = "in-rust-tree", ...)]` line.
  > applied by db-expand-next1: `rg '\bunsafe\b' crates/base-db` finds only the two salsa options; added. hir-ty-borrowck already forbids `unsafe_code` under salsa's macros, so their expansions pass.

### hir-expand

- `crates/hir-expand/src/lib.rs:8` crate: no `unsafe` in its compiled code. The `unsafe` in `builtin/attr_macro.rs:111-154` is token text inside `quote::quote!` (the code `#[global_allocator]` expands to in the user's crate), not code of hir-expand; `lib.rs:1578` is the salsa `unsafe(no_lifetime)` option. Add `#![forbid(unsafe_code)]` after `#![allow(rustdoc::private_intra_doc_links)]`.
  > applied by db-expand-next1: checked; the only `unsafe` is quoted tokens in `attr_macro.rs` and the salsa option. Added.

### hir

- `crates/hir/src/lib.rs:634` block (`Module::nearest_non_block_module`, `unsafe { id.to_static() }`): no SAFETY. The block shares its line with `Module { id: ... }`, so the comment goes on the line above it:
  ```rust
  // SAFETY: `to_static` needs the id not to outlive the query computation it was made in. `id` is `self.id` or a module containing it, read from `db`'s def maps, and `Module` holds it as `'static` as every `hir` handle does, used only with the database it came from. A `Module` kept past the revision that removed its module would name a freed salsa slot.
  ```
- `crates/hir/src/check.rs:128`, `:162`: ok.
  > applied by db-ty (18:55): a SAFETY above the line. It adds that salsa interned ids are never
  > freed now, so the `'static` id names the same module for the database's life.

### hir-def (db's files)

The `to_static`/`to_db` calls all follow the house style `expr_store/lower.rs:2808` and `:3085` already use.

- `crates/hir-def/src/lib.rs:657` block (`BlockIdLt::to_static`, a `transmute`): no SAFETY. Proposed: `` // SAFETY: `BlockIdLt<'db>` and `BlockId` differ only in a lifetime, so they have one layout and the bits are kept. The lifetime is what the caller answers for (`# Safety`): the id must not outlive the query computation it was made in, or it may name a freed salsa slot. `` Its `# Safety` also says "the `ModuleId`" where it means "the `BlockId`".
  > applied by db-expand-next1: the block already had a true SAFETY (same meaning, its words kept); fixed the `# Safety` to say `BlockId`.
- `crates/hir-def/src/lib.rs:665` block (`BlockId::to_db`): no SAFETY. Proposed: `` // SAFETY: the same type but for the lifetime, as in `to_static`. The caller makes `self` an id of `_db` (`# Safety`), so `'db`, the borrow of `_db`, is its lifetime. An id of another database would name a slot of another table. ``
  > not applied: `lib.rs:671` already has a SAFETY saying the same (one type up to the lifetime; that `_db` made it is the caller's, per `# Safety`), checked true.
- `crates/hir-def/src/lib.rs:694` block (`ModuleIdLt::to_static`): no SAFETY. Proposed: as for `lib.rs:657` with `ModuleIdLt<'db>`/`ModuleId`.
  > not applied: `lib.rs:703` already has the equivalent SAFETY, checked true.
- `crates/hir-def/src/lib.rs:747` block (`ModuleId::to_db`): no SAFETY. Proposed: as for `lib.rs:665` with `ModuleId`.
  > not applied: `lib.rs:759` already has the equivalent SAFETY, checked true.
- `crates/hir-def/src/find_path.rs:311`, `:328`, `:2616`, `:2634` blocks (`module_id.to_static()` into an `ItemInNs`): no SAFETY. Proposed, on the line above each block's line: `` // SAFETY: `to_static` needs the id not to outlive the query computation it was made in. The `'static` id only keys this lookup (`find_in_scope`/`find_in_prelude` compare it with the def map's items) and is dropped before `find_path` returns; the `Choice` holds names, not ids. If it were stored, a later revision could read a freed salsa slot through it. ``
  > applied by db-expand-next1: replaced the generic comment at all four. Checked `ItemScope::name_of`/`names_of` only binary-search the scope's own entries. Each comment names the one helper that site calls; the `find_in_scope` sites say the name it gives is not an id, and the main `Choice` holds segments of names.
- `crates/hir-def/src/nameres.rs:552` block (`block_def_map`, `block_id.to_static()`): no SAFETY. Proposed: `` // SAFETY: `to_static` needs the id not to outlive the query computation it was made in. `block_id` is this query's own key, and the `'static` copy goes only into the `DefMap` this query returns, which salsa invalidates with the key. Stored anywhere else, it could name a freed salsa slot in a later revision. ``
  > applied by db-expand-next1, adding "and the modules it makes" (`DefMap::empty` puts the block into the root `ModuleIdLt::new`).
- `crates/hir-def/src/nameres.rs:554` block (`block_id.module(db).to_static()`): no SAFETY. Proposed: `` // SAFETY: as for `block_id` above: the block's module goes only into the `DefMap` this query returns. ``
  > applied by db-expand-next1.
- `crates/hir-def/src/nameres.rs:593` block (`DefMap::empty`, `ModuleIdLt::new(..).to_static()`): no SAFETY. Proposed: `` // SAFETY: `to_static` needs the id not to outlive the query computation it was made in. `empty` runs only inside the queries that build a def map (`crate_def_map`, `block_def_map`, and `frozen`'s decode within them), and the root id goes only into the `DefMap` that query returns. Stored elsewhere, it could name a freed salsa slot in a later revision. ``
  > applied by db-expand-next1, naming the real query: `crate_def_map` is a plain function over `crate_local_def_map`, which runs `frozen::served`'s decode.
- `crates/hir-def/src/nameres.rs:661` block (`DefMap::module`, `id.to_static()`): no SAFETY. Proposed: `` // SAFETY: `to_static` needs the id not to outlive the query computation it was made in. The `'static` copy only keys the lookup in `self.modules` and is dropped when this returns; nothing stores it. ``
  > applied by db-expand-next1, corrected: "nothing stores it" is false. In a collected map the id is the argument of the tracked `Modules::module(self, db, module: ModuleId)`, whose key salsa keeps. The comment says so and why that is sound: the id is only hashed and compared there, never read through, and salsa ids carry a generation, so a freed module's id matches no later one.
- `crates/hir-def/src/nameres.rs:1288` block (`Index for ModulesMap`, `id.to_static()`): no SAFETY. Proposed: on the line above `.get(...)`, `` // SAFETY: as in `DefMap::module`: the `'static` copy only keys this lookup and is dropped when it returns. ``
  > applied by db-expand-next1 (true here: a plain `FxHashMap::get`).
- `crates/hir-def/src/visibility.rs:63` block (`Visibility::is_visible_from`, `m.to_db(db)`): no SAFETY. Proposed: `` // SAFETY: `to_db` needs `m` to be an id of `db`. A `Visibility` is resolved against `db`'s def maps (`Visibility::resolve`, the collector) and asked only with the same database, so `m` is one of `db`'s modules. A module of another database would name a slot of another table. ``
  > applied by db-expand-next1, keeping the old comment's note that the `'db` copy is read only during the call.
- `crates/hir-def/src/attrs/docs.rs:367` block (`as_mut_vec().clear()` in the guard's `drop`): no SAFETY. Proposed: `` // SAFETY: `as_mut_vec` needs the bytes to be UTF-8 again when the borrow ends; `clear` leaves none. The guard runs only when `remove_indent` unwound partway through, when the bytes may not be UTF-8, which is why `String::clear` is not used. ``
  > not applied: no longer there. `remove_indent` (HEAD and the tree) now moves the text out (`mem::take(..).into_bytes()`), compacts the `Vec<u8>`, and makes the `String` again with one checked `String::from_utf8`. `attrs/docs.rs` has no `unsafe`, and the guard clears a `String` that is already empty.
- `crates/hir-def/src/attrs/docs.rs:437` block (`as_bytes_mut().copy_within(..)`): no SAFETY. See the MAY BE UNSOUND note below; if kept as is: `` // SAFETY: `as_bytes_mut` needs the bytes to be UTF-8 again before the `str` is used. This moves the line, without its indent, down to `copy_into`, so the bytes from there to the next line's start may split a character until the `truncate` below; on a panic the guard clears the string. ``
  > not applied: the block is gone (safe `Vec::copy_within`, see above).
- `crates/hir-def/src/attrs/docs.rs:465` block (`as_mut_vec().truncate(copy_into)`): no SAFETY. Proposed: `` // SAFETY: `[0, copy_into)` is the lines copied down, each cut at a character boundary (`indent_size` is a `char_indices` offset), so what is left is UTF-8. ``
  > not applied: the block is gone (safe `Vec::truncate`, then `String::from_utf8`).
- `crates/hir-def/src/attrs/docs.rs:419-443` MAY BE UNSOUND (a library contract, not memory): between `as_bytes_mut` (`:437`) and the `truncate` (`:465`), the loop keeps using `guard.0.docs` as a `str` (`TextSize::of(&guard.0.docs)` and `&guard.0.docs[range]` at `:421`/`:424`) while the bytes between the copied text and the next line can be a split UTF-8 sequence. That breaks `str::as_bytes_mut`'s contract ("valid UTF-8 before the borrow ends and the underlying `str` is used"). In practice each read touches only untouched lines and boundaries, so nothing reads the bad bytes. Fix without changing behavior: take `let bytes = unsafe { guard.0.docs.as_mut_vec() }` once before the loop, slice lines from `bytes` (each untouched range is UTF-8: `str::from_utf8_unchecked` with its own SAFETY), and use `bytes.len()` for the length.
  > not applied as proposed, because the code is already sound with no `unsafe`. Line lengths come from the source map and `docs_len` taken before the move, not from a `str`. Each line's indent is cut with `char_starts` over its bytes, which no copy has reached yet. The one UTF-8 check, `String::from_utf8`, is a single pass that SIMD-validates the result. The unsafe form would save only that pass, so `unsafe.md` ("only where it buys measured speed") keeps it safe. The safe-reference test asked for already exists: `attrs/docs.rs` `remove_indent_as_by_lines` checks equal bytes against `remove_indent_by_lines`, a dedent built into a new `String` with `char_indices`. It runs over 2000 xorshift cases of ASCII, `ü`, `日本` and `🦀`, tabs, blank and whitespace-only lines, and lines shorter than the indent, with sugared line, sugared block and desugared kinds mixed.
- `crates/hir-def/src/nameres/collector.rs:3249` block (`ModuleIdLt::new(..).to_static()` for a new child module): no SAFETY. Proposed: `` // SAFETY: `to_static` needs the id not to outlive the query computation it was made in. The collector runs inside the def-map query (`crate_def_map`/`block_def_map`), and the id goes only into the `DefMap` it builds (`modules`, the parent's children, the scope), which salsa invalidates with that query. Stored elsewhere, it could name a freed salsa slot in a later revision. ``
  > applied by db-expand-next1, naming `crate_local_def_map` (the tracked query) for `crate_def_map`.
- `crates/hir-def/src/nameres/demand.rs:209`, `:216`, `:224` blocks (`block_owner`, `to_static` of a module and an outer block): no SAFETY. Proposed, above `:209`: `` // SAFETY: `to_static` needs the id not to outlive the query computation it was made in. `module` and `outer` are locals of this walk, only asked for their def maps and blocks with `db`, and dropped before `block_owner` returns a `DefWithBodyId`; nothing stores them. `` Above `:216` (inside the match arm, the line before it) and `:224`: `` // SAFETY: as for `module` above: a local of this walk, never stored. ``
  > applied by db-expand-next1, corrected the first comment: `owner_in_module` calls `DefMap::module`, which keys the `Modules::module` query with the id (only hashed and compared). The comment says so instead of "nothing stores them".
- `crates/hir-def/src/nameres/frozen.rs:996` block (`block.to_static()` while encoding a module): no SAFETY. Proposed: `` // SAFETY: `to_static` needs the id not to outlive the query computation it was made in. `block` only finds its owner (`block_owner`), whose path is what gets written, and is dropped when this returns. ``
  > applied by db-expand-next1, corrected: it is not dropped when this returns. `Enc::block_owner` caches it in the encoder's `block_owners`, and it also reads its ast id and its `block_def_map`. The comment says the encoder is a local of one encoding (`encode_maps` and the other two `Enc::new` sites, consumed by `finish`), so the cache dies with it.
- `crates/hir-def/src/nameres/frozen.rs:2922` block (`ModuleIdLt::new(..).to_static()` while decoding a def map): no SAFETY. Proposed, above `let module =`: `` // SAFETY: `to_static` needs the id not to outlive the query computation it was made in. The decode runs inside the def-map query that reads the frozen def map, and the ids go only into the `DefMap` it returns (`self.own`, then its modules), which salsa invalidates with that query. ``
  > applied by db-expand-next1, naming the query (`crate_local_def_map`, through `served`, the only caller of `decode`).
- `crates/hir-def/src/dyn_map.rs:187`, `:194`, `nameres/tests/paths.rs:772-794`: ok. `lib.rs:407` `#[salsa::interned(unsafe(no_lifetime))]`: a salsa option, not Rust unsafe.

### hir-ty (db's files)

Update 18:55: `next_solver/interner.rs`'s `mod table` was rewritten while this was written (one
op per block, `wrapping_add`, the `Erase` trait, safe `Rows::kind`/`flags`/`binder`), so the
items below for `interner.rs:2754`, `:2786`, `:2800`, `:2814`, `:2828`, `:2844`, `:3129`,
`:3274`, `:3308`, `:3432`, `:3452` are done, and the five `kind()` splits (`interner.rs:900`,
`ty.rs:926`, `consts.rs:269`, `region.rs:260`, `predicate.rs:633`) need nothing once
`X.kind(..)` is the safe call: each block is then one `transmute` (or none, through `Erase`).
The new `mod table` passes: `Erase`'s `# Safety`, the `erase!` and test `erase_static!` impls'
comments, and every block. The items for `:266`, `:403`, `:404`, `:411` and `solver.rs:42`
still stand.

> applied by db-ty (18:55):
> - `:266`: the SAFETY there already says it (a row number that borrows nothing, never freed,
>   valid in every revision).
> - `:403`/`:404`: both impls deleted, replaced by a `const` assertion that the auto traits give
>   `Send + Sync`.
> - `:411` `conjure`: now an `unsafe fn` with `# Safety` (the interner must not outlive the
>   attachment on the stack). `infer.rs` takes the path context's interner instead. hir's six
>   callers and `def_id.rs`'s `Debug` have SAFETY comments; deleting it needs hir's public
>   `Type` API to take a database.
> - `solver.rs:42`: the SAFETY now names the layout invariant, and the transmute names its types.
>
> applied by db-ty-next1 (19:10), `:411` finished: `DbInterner::conjure` is deleted.
> - Its callers in `generic_arg.rs` were already gone, and `infer.rs` takes the path context's
>   interner.
> - hir's `Type::instantiate_with_errors`, `instantiate`, `instantiate_type_params`, `unknown`
>   and `new_unit` take `db` and use `DbInterner::new_no_crate(db)`, the same interner
>   `conjure` made. Every caller in `crates/hir/` passes it, including term_search and its
>   `LookupTable::insert`.
> - `Callable` erases its signature's bound regions once, in `as_callable`, into a field, so
>   `erased_sig` is gone.
> - `SolverDefId`'s `Debug` runs inside `with_attached_db` and borrows the database only for
>   the closure.
> - The `with` SAFETY comment no longer mentions `conjure`.
> - Callers outside db's paths: six in `blaze-diagnostics` (`v2/requests/diagnostics.md`) and
>   one in `blaze-doc` (`v2/requests/speed.md`).
> - solver-sync-interner-next1, which holds `interner.rs`/`def_id.rs` for its port, was told.
>
> The crate-root items are already done: hir-ty-wfcheck has `#![forbid(unsafe_code)]` (18:55)
> and hir-ty-borrowck had it already. hir and hir-ty keep `unsafe` modules, so they get none.

- `crates/hir-ty/src/next_solver/interner.rs:266` `unsafe impl salsa::SalsaValue for $stored_name` (in `impl_stored_interned!`): its comment says only that it is safe. Proposed: `` // SAFETY: a `$stored_name` is a row index (a `NonZeroU32`) into a process-wide table that is never freed or rewritten, so one kept from an older revision names the same value in every newer one, and it holds no `'db` reference (what `SalsaValue`'s `# Safety` forbids). `$name` is not stored: its `'db` marks where it is meant to be used. ``
- `crates/hir-ty/src/next_solver/interner.rs:403`, `:404` `unsafe impl Send`/`Sync for DbInterner<'_>`: no SAFETY, only `// FIXME: very wrong` (rust-lang/rust#144808). Every field is already `Send + Sync`: `&'db dyn HirDatabase` (blaze's `salsa::Database: Send + Sync`, `vendor/salsa/src/database.rs:38`), `Option<&'db LangItems>` (ids), `Option<Crate>`, `u32`, `tls_cache::Solved` (a `u64`) and `bool`. So the impls assert only what the compiler derives. Preferred: delete both impls and the FIXME (no behavior change, and the build then proves it). Otherwise: `` // SAFETY: every field is `Send` and `Sync` (`&dyn HirDatabase` because `salsa::Database: Send + Sync`; the rest are ids and integers), so this only states what the auto trait would derive. A field that is not would make an interner moved to another thread a data race. `` and `` // SAFETY: as for `Send`: every field is `Sync`. ``
- `crates/hir-ty/src/next_solver/interner.rs:411` block (`DbInterner::conjure`, `transmute::<&dyn HirDatabase, &'db dyn HirDatabase>`): no SAFETY, and MAY BE UNSOUND. A safe `pub fn` stretches the attached database's borrow (valid only inside `with_attached_db`'s closure) to any `'db` the caller picks, and returns it in the interner, so nothing ties the interner to the attach scope. Callers: `infer.rs:2628` and `generic_arg.rs:611`, `:617`. A sound shape is `conjure` taking an `op: impl FnOnce(DbInterner<'_>) -> R` run inside `with_attached_db`, or callers passing the interner they have. No comment can be written that holds as is.
- `crates/hir-ty/src/next_solver/interner.rs:900` block (`Pattern::kind`): 2 ops (`PATTERNS.kind`, `transmute`). Split:
  ```rust
  // SAFETY: a `Pattern` is a row of `PATTERNS` (made by `intern`, or by `at` from a row `published` accepted).
  let kind = unsafe { PATTERNS.kind(self.index) };
  // SAFETY: the two types differ only in their lifetime, which an interned value does not depend on (as in `intern`).
  unsafe { std::mem::transmute::<PatternKind<'static>, PatternKind<'db>>(kind) }
  ```
- `crates/hir-ty/src/next_solver/ty.rs:926` (`Ty::kind`), `consts.rs:269` (`Const::kind`), `region.rs:260` (`Region::kind`), `predicate.rs:633` (`Predicate::kind`) blocks: 2 ops each (`X.kind(self.index)`, `transmute`). Split each exactly as `interner.rs:900`, with `TYS`/`CONSTS`/`REGIONS`/`PREDICATES` and the kind type.
- `crates/hir-ty/src/next_solver/interner.rs:2754` block (`Column::allocate`): 2 ops (`alloc_zeroed` and `alloc`, one per branch). Split:
  ```rust
  let fresh = if self.zeroed {
      // SAFETY: `layout` is not zero-sized (the assert above); `alloc_zeroed` of a zero-sized layout is undefined behavior.
      unsafe { alloc::alloc_zeroed(layout) }
  } else {
      // SAFETY: as for `alloc_zeroed`: `layout` is not zero-sized.
      unsafe { alloc::alloc(layout) }
  }
  .cast::<T>();
  ```
- `crates/hir-ty/src/next_solver/interner.rs:2786` block (`Column::write`): 2 ops (`add`, `write`). Split:
  ```rust
  // SAFETY: `locate` gives an `offset` below `segment_len(segment)`, the length `allocate` gave the segment, and `at < 2^32` (the caller's) keeps `segment` below `SEGMENTS`, so the pointer stays in the segment's allocation.
  let place = unsafe { self.segment(segment).add(offset) };
  // SAFETY: `place` is in bounds and aligned for `T`, and the position is the caller's alone and not written yet, so no reader sees it half-written and nothing is overwritten without a drop.
  unsafe { place.write(value) }
  ```
- `crates/hir-ty/src/next_solver/interner.rs:2800` block (`Column::place`): 2 ops (`add`, deref). Split:
  ```rust
  // SAFETY: as in `write`: `offset` is within the segment, which `segment()` allocated if need be.
  let place = unsafe { self.segment(segment).add(offset) };
  // SAFETY: the segment is zeroed and `T` is an atomic whose zero bits are a value (the caller's precondition), and segments are never freed, so `place` is a `T` for the column's life.
  unsafe { &*place }
  ```
- `crates/hir-ty/src/next_solver/interner.rs:2814` block (`Column::peek`): 2 ops (`add`, deref). Split inside the closure:
  ```rust
  (!base.is_null()).then(|| {
      // SAFETY: `offset` is within the segment `base` starts (`locate`).
      let place = unsafe { base.add(offset) };
      // SAFETY: as in `place`: zeroed memory of an atomic `T`, never freed.
      unsafe { &*place }
  })
  ```
- `crates/hir-ty/src/next_solver/interner.rs:2828` block (`Column::get`): 2 ops (`add`, deref). Split:
  ```rust
  let base = self.segments[segment].load(Ordering::Relaxed);
  // SAFETY: `at` was written before this call (the caller's precondition), and that write allocated the segment, so `base` is it and `offset` is within it.
  let place = unsafe { base.add(offset) };
  // SAFETY: the value at `place` was written before this call and is never written again or freed.
  unsafe { &*place }
  ```
- `crates/hir-ty/src/next_solver/interner.rs:2844` block (`Column::slice`): 2 ops (`add`, `from_raw_parts`). Split:
  ```rust
  let base = self.segments[segment].load(Ordering::Relaxed);
  // SAFETY: the run starts at `offset` in this segment, which the write of its first item allocated (the caller's precondition).
  let first = unsafe { base.add(offset) };
  // SAFETY: the `len` values from `first` are one run in one segment (`Run::claim`), all written before this call and never written again or freed.
  unsafe { std::slice::from_raw_parts(first, len) }
  ```
- `crates/hir-ty/src/next_solver/interner.rs:3129` block (`Rows::add`): 4 ops (three `Column::write`, `Column::place`). Split, in this order:
  ```rust
  // SAFETY: `row` is ours alone (the `fetch_add` above) and at most `self.last`, a `u32`, so below 2^32; `vacant.insert` publishes it only after these writes. A row written twice could be read torn.
  unsafe { self.kinds.write(row, kind) };
  // SAFETY: as for `kinds`: `row` is ours alone, below 2^32 and not published yet.
  unsafe { self.flags.write(row, info.flags.bits()) };
  // SAFETY: as for `kinds`.
  unsafe { self.binder.write(row, info.binder.as_u32()) };
  // SAFETY: the tag column is `Column::zeroed` and of `AtomicU8`s (`Rows::new`), and `row` is below 2^32. The release store publishes the writes above.
  unsafe { self.tag.place(row) }.store(info.tag + 1, Ordering::Release);
  ```
- `crates/hir-ty/src/next_solver/interner.rs:3274` block (`Lists::put`): 5 ops (the items' `write`, `start.write`, `flags.write`, `binder.write`, `len.place`). Split:
  ```rust
  for (i, &item) in items.iter().enumerate() {
      // SAFETY: position `start + i` is in the run `claim` gave this thread alone, below 2^32 and not written yet.
      unsafe { self.items.column.write(start + i as u64, item) };
  }
  // SAFETY: `row` is ours alone (the `fetch_add`), at most `LAST_ROW`, so below 2^32, and `vacant.insert` publishes it only after these writes.
  unsafe { self.start.write(row, start as u32) };
  if let Some((flags, binder)) = info {
      // SAFETY: as for `start`: `row` is ours alone and not published yet.
      unsafe { self.flags.write(row, flags.bits()) };
      // SAFETY: as for `start`.
      unsafe { self.binder.write(row, binder.as_u32()) };
  }
  // SAFETY: the length column is `Column::zeroed` and of `AtomicU32`s (`Lists::new`), and `row` is below 2^32. The release store publishes the row.
  unsafe { self.len.place(row) }.store(len, Ordering::Release);
  ```
- `crates/hir-ty/src/next_solver/interner.rs:3308` block (`Lists::get`): 3 ops (`len.get`, `start.get`, `items.column.slice`). Split:
  ```rust
  // SAFETY: the row was published before this call (the caller's precondition), and the release store of its length published everything else.
  let len = unsafe { self.len.get(row) }.load(Ordering::Relaxed) - 1;
  // SAFETY: as for `len`: the row's start was written before its length was published.
  let start = unsafe { *self.start.get(row) };
  // SAFETY: the row's `len` items from `start` were written before it was published, in one run (`Run::claim`).
  unsafe { self.items.column.slice(u64::from(start), len as usize) }
  ```
- `crates/hir-ty/src/next_solver/interner.rs:3432` block (test `a_row_interned_twice_is_one_row`): 2 ops (`ROWS.kind`, `ROWS.tag`). Split: `` // SAFETY: `row` came from `ROWS.intern`. `` / `assert_eq!(unsafe { ROWS.kind(row) }, kind);` / `` // SAFETY: as for `kind`. `` / `assert_eq!(u32::from(unsafe { ROWS.tag(row) }), kind.0);`
- `crates/hir-ty/src/next_solver/interner.rs:3452` block (test `a_row_keeps_its_flags`): 3 ops (`flags`, `binder`, `tag`). Split into three `assert_eq!`s, each with its own `unsafe { .. }` around the one call and a `` // SAFETY: `row` came from `ROWS.intern`. `` above it.
- `crates/hir-ty/src/next_solver/solver.rs:42` block (`From<&InferCtxt> for &SolverContext`): the comment is only `` `repr(transparent)` ``. Proposed: `` // SAFETY: `SolverContext` is `#[repr(transparent)]` over its one field, an `InferCtxt<'db>`, so the two references have one layout and validity, and `'a` is kept. A second non-zero-sized field would make this read past the value. ``
- Every other block and impl in `next_solver/` (`interner.rs` macros at `:127`, `:145`, `:147`; `persist.rs:511` (`assemble!`, in a macro, so `multiple_unsafe_ops_per_block` skips it), `:1776`; `tls_cache.rs:53`, `:70`; `generic_arg.rs`, `allocation.rs`, `valtree.rs`, `opaques.rs`, the `flags`/`binder` blocks) and `lower.rs:2360`, `lower/persist.rs:192`: ok.

### hir-ty-wfcheck

- `crates/hir-ty-wfcheck/src/lib.rs:12` crate: no `unsafe` anywhere (`lib.rs`, `outlives.rs`, `persist.rs`). Add `#![forbid(unsafe_code)]` after the `//!` docs, before `extern crate rustc_type_ir;`.
  > applied by db-ty.

### hir-ty-borrowck

- Already `#![forbid(unsafe_code)]` (`lib.rs:14`); no unsafe. Nothing to do.

## speed-hir-store-ty: cache the block traits in scope per block (for speed-hir-nameres-paths' finding)

From `v2/requests/speed-hir.md` "## for speed-hir-store-ty (from speed-hir-nameres-paths)". Today
`InferenceContext::get_traits_in_scope` collects a new `FxHashSet<TraitId>` on every method call
inside a block that imports or declares a trait: it copies the crate's whole set, then adds the
block's. After this edit the set is filled once each time the walk enters such a block (keyed on
`self.resolver.module()`: a block with items is a module of its own, and so is its chain of
enclosing blocks). Every method call inside the block then gets an `Rc` clone, a refcount bump
with no allocation and no hashing. `Rc::make_mut` refills the same table in place. No
`RefCell` borrow outlives the call, so a nested method call inside a probe is fine; if a caller
still holds the old `Rc`, `make_mut` copies once. The results are the same set as before.
`InferenceContext` is per body and single-threaded, like its existing `OnceCell`/`RefCell` uses.

> forwarded to db: `crates/hir-ty/src/infer.rs` is db-ty's; the `method_resolution.rs` half is done.

### crates/hir-ty/src/infer.rs  (3 edits; owner db-ty)
- ~L1656, the field  `    traits_in_scope: FxHashSet<TraitId>,`  ->
  ```
      traits_in_scope: FxHashSet<TraitId>,
      /// The traits in scope in the innermost block module that adds some: which module, whether
      /// it adds any, and `traits_in_scope` together with the block's.
      block_traits: RefCell<(Option<hir_def::ModuleId>, bool, std::rc::Rc<FxHashSet<TraitId>>)>,
  ```
- ~L1761, the initializer  `            traits_in_scope: resolver.traits_in_scope(db),`  ->
  ```
              traits_in_scope: resolver.traits_in_scope(db),
              block_traits: RefCell::new((None, false, std::rc::Rc::default())),
  ```
- ~L2896, replace the whole function `pub(crate) fn get_traits_in_scope(&self) -> Either<FxHashSet<TraitId>, &FxHashSet<TraitId>> { .. }` with:
  ```
      pub(crate) fn get_traits_in_scope(
          &self,
      ) -> Either<std::rc::Rc<FxHashSet<TraitId>>, &FxHashSet<TraitId>> {
          let module = self.resolver.module();
          let mut cache = self.block_traits.borrow_mut();
          let (key, has_block_traits, traits) = &mut *cache;
          if *key != Some(module) {
              *key = Some(module);
              let mut b_traits = self.resolver.traits_in_scope_from_block_scopes(self.db).peekable();
              *has_block_traits = b_traits.peek().is_some();
              if *has_block_traits {
                  let set = std::rc::Rc::make_mut(traits);
                  set.clear();
                  set.extend(self.traits_in_scope.iter().copied());
                  set.extend(b_traits);
              }
          }
          if *has_block_traits {
              Either::Left(traits.clone())
          } else {
              Either::Right(&self.traits_in_scope)
          }
      }
  ```

### crates/hir-ty/src/method_resolution.rs  (1 edit; lane items' file, held now by speed-types-probe-next1)
- ~L195 in `with_method_resolution`  `Either::Left(it) => it,`  ->  `Either::Left(it) => &**it,`
  (`it` is `&Rc<FxHashSet<TraitId>>`; both arms must be `&FxHashSet<TraitId>`.)

This is the only caller (`rg get_traits_in_scope crates/`). Status: applied (both halves; see below)

> method_resolution.rs edit: done by speed-types-probe, as
> `Either::Left(it) => std::borrow::Borrow::<FxHashSet<TraitId>>::borrow(it),`, which compiles
> with `get_traits_in_scope` returning either `FxHashSet` (today) or `Rc<FxHashSet>` (after the
> infer.rs edits), so the infer.rs part can land on its own. The infer.rs part is still open.

> infer.rs part applied by db-ty (18:55), as written.

## speed-hir-store: id-only scans of a store (from speed-hir-store)

With expressions as columns, `store.exprs()` decodes every node. `ExpressionStore` now also has
`expr_ids()` / `pat_ids()` (ids in order, nothing decoded; `ExactSizeIterator`) and
`expr_count()` / `pat_count()`. Where a scan wants only ids, use them:
- `crates/hir-ty-borrowck/src/regions.rs` `synthesized()` (~L825) and `path_arguments`
  (~L2085): `store.exprs().map(|(id, _)| id)` / `for (id, _) in store.exprs()` ->
  `store.expr_ids()` / `for id in store.expr_ids()`.
- The seven whole-store `exprs()` scans per body in `hir-ty-borrowck` (`matched`, `tested_at`,
  `union_literals`, `guard_borrows`, `scrutinee_of`, `guard_bindings`, `statement_values`):
  better done once per body into one pass that fills all seven, since each decodes every node.
Found by speed-hir-store-ty.

> forwarded to db: `crates/hir-ty-borrowck/` is lane db's.

> partly applied by db-ty (18:55):
> - `synthesized` and `path_arguments` use `expr_ids()`, and the `parent` map is preallocated
>   with `expr_count()`.
> - Open: fusing the seven whole-store scans into one pass per body. That is a refactor of
>   seven functions across `check.rs`, `liveness.rs` and `lib.rs`, left for the next borrowck
>   pass.

> applied by db-ty-d-borrowck: `hir-ty-borrowck/src/scan.rs` `Scan::new` walks `store.exprs()`
> once per owner (the store its closures, coroutines and inline consts share), not once per
> table and per body, and fills all seven (`matched` and `union_literals` as bitsets,
> `tested_at` dense by `PatId`, the store half of `guard_borrows`, `scrutinee_of`,
> `guard_bindings`, `statement_values`) plus regions' three per-body scans (`lets`,
> `pattern_paths`, `path_arguments`'s paths with their closure; its parent map is now built
> only when a path spells generics). `Ctx::scan` carries it. Test: `scan/tests.rs` checks every
> table against the old scans, copied verbatim, over a fixture that gives each entries.

## speed-hir-store: `ExpressionStoreDiagnostics::InactiveCode` loses `opts` (from speed-hir-store)

Each cfg'd-out field, parameter or statement used to copy the crate's whole `CfgOptions` hash set
into the store's diagnostics. The diagnostic also encoded it, and `moved()` copied it again
whenever an edit elsewhere in the file moved the source map. The options are always the crate's,
so the variant is now `InactiveCode { node, cfg }` (hir-def's lowering, `signatures.rs` and the
encoding are already done; source map schema v3). One edit in `crates/hir/src/diagnostics.rs`
(~L1749), reading them where it reports, as the `UnconfiguredCode` arm at ~L875 already does:

```rust
// before
ExpressionStoreDiagnostics::InactiveCode { node, cfg, opts } => {
    InactiveCode { node: *node, cfg: cfg.clone(), opts: opts.clone() }.into()
}
// after
ExpressionStoreDiagnostics::InactiveCode { node, cfg } => {
    InactiveCode { node: *node, cfg: cfg.clone(), opts: self.krate.cfg_options(self.db).clone() }
        .into()
}
```

From speed-hir-defs-sigs' finding. Other matches on the variant use `..` (`hir-ty-borrowck/src/lib.rs:179`,
`blaze-diagnostics/.../asm_labels.rs:125`) and need no edit.

> forwarded to db: `crates/hir/src/diagnostics.rs` is lane db's; hir-ide and ide-db match no `InactiveCode`.

> applied by db-ty (18:55).

## speed-hir-store: `Path::BarePath` holds a `ModPath`, not an `Interned<ModPath>` (from speed-hir-store)

`hir_expand::mod_path::ModPath` is now a 16-byte `Copy` value with interned segments
(speed-hir-expand). hir-def's `Path::BarePath(ModPath)` and `NormalPath::mod_path: ModPath` no
longer intern it a second time. One construction outside hir-def, `crates/hir/src/semantics.rs`:

```rust
// ~L2843, before
&Path::BarePath(Interned::new(ModPath::from_segments(kind, segments))),
// after
&Path::BarePath(ModPath::from_segments(kind, segments)),
```

and its import at ~L43, `use intern::{Interned, Symbol, sym};` -> `use intern::{Symbol, sym};`
(that line was the file's only use of `Interned`). The reads (`if let Path::BarePath(path) = path`
at ~L2912) bind `&ModPath` and need no edit.

> forwarded to db: `crates/hir/src/semantics.rs` is lane db's; hir-ide builds no `Interned<ModPath>`.

> applied by db-ty (18:55), the `Interned` import with it.

## 8. Answer to codegen's request 3 (run codegen as a query): declined as written (db, 19:55)

Not a request to apply; codegen's lane and the daemon (cargo) should read it.

A `blaze_query` query keyed by the program's roots that answers `Built` would be a stale-code
bug. `HirFrontend` reads the front end through salsa, and none of those reads becomes an edge
of the query. Its result depends on every source file, so its memo could be reused after an
edit (AGENTS.md: a cache key must miss no input). `HirFrontend<'db>` also borrows the salsa
database, so no `'static` service slot on `Cx` could hold it soundly.

Codegen needs nothing from blaze-query for this. Every artifact is already content-addressed
in the store by `query::keys` (instance key, settings and policy). `driver::Lookup` is that
cache, and the front end's own incremental reuse is salsa's. So the daemon calls
`driver::Build::new(backend, frontend, policy, &shared_memo()).run(store, ..)` directly, as a
plane batch after the front end, and writes `Built::puts` back.

Kinds `0x4000..0x4007` stay reserved. They become `Query` impls in `blaze-codegen` once the
front end's queries run on blaze-query, each keyed by its `InstanceKey`. Their `execute` then
reaches the front end through `Cx::get`, so every read is an edge and no services are needed.

> answered: by db. Sent to codegen (window `codegen-next1`).
> applied: noted; db's answer to codegen (codegen is told by db).

## 9. Coordinator: two new front-end crates, `hir-ty-matchck` and `hir-ty-unsafeck` (db, 20:05)

What:
- In the root `Cargo.toml` `[workspace.dependencies]`, beside `hir-ty-borrowck`:
  `hir-ty-matchck = { path = "crates/hir-ty-matchck", version = "0.0.0" }` and
  `hir-ty-unsafeck = { path = "crates/hir-ty-unsafeck", version = "0.0.0" }`. Both are members
  already through `crates/*`.
- In `docs/v2/plan/reference/crates.md`, add both crates to lane db's paths (line 71), and to
  the table at line 111 beside `hir-ty-borrowck`.

Why: this answers the diagnostics request "diagnostics-lints-mir-unsafe", whose note says "the
coordinator names the front-end crate it moves to first". It also answers "what match checking
lints" and "what match checking gives", and db-requests' D3. rustc's `check_match` and
`check_unsafety` are language semantics (semantics-in-front-end). Today `hir` calls them from
`hir-ide`, which is deleted at integration, and `hir-ide`'s match check makes every literal,
range and constant `_`. The ports are rustc's `rustc_mir_build` `thir/pattern/check_match.rs`
(over the workspace's `ra-ap-rustc_pattern_analysis`) and `check_unsafety.rs`. Children
db-matchck and db-unsafeck write them (briefs `db-matchck.md` and `db-unsafeck.md`). `hir`
then depends on both crates, and stops calling `hir_ide::diagnostics::{missing_unsafe, ..}`
and hir-ide's match check.

> applied in part by requests: `hir-ty-unsafeck = { path = "crates/hir-ty-unsafeck", version = "0.0.0" }` beside `hir-ty-borrowck` (the crate is complete; new to the lock). `hir-ty-matchck`'s line followed once the crate had its `src/` (pass 12). The `crates.md` entries are the coordinator's: listed in requests' report.

## 10. Lane db's decisions on db-requests' D1 to D10 (db, 20:05)

Notes, nothing to apply. The full list is in `~/restart-2026-10-28/briefs/v2/db-dispatch/db.md`.
- D1: already done. `hir-ty`'s interner implements `with_global_cache(scope, f)` and
  `cache_scope`.
- `crates/hir-ty/src/next_solver/`: solver-sync keeps the 18 files db-ty-types handed it.
  db-ty-next1 took solver-sync's request 3 at 19:15, so its children db-ty-port-infer and
  db-ty-port-solver own `infer/`, `solver.rs`, `fulfill.rs`, `normalize.rs`,
  `structural_normalize.rs`, `obligation_ctxt.rs`, `inspect.rs`, `format_proof_tree.rs` and
  `tls_cache.rs`. db-ty-next1's dispatch tasks in solver-sync's files (`interner.rs`,
  `def_id.rs`, `generic_arg.rs`) go to `solver-sync-inbox.md`.
- D2: db-ty-next1 edits the four lines in `method_resolution.rs`, `probe.rs` and `confirm.rs`,
  after a tmux to speed-types-probe-next1, which holds the file.
- D3: request 9.
- D4: request 8.
- D5: see request 11.
- D6: an evicted `infer` result is dropped and recomputed, and its fingerprint keeps cutoff
  (sound, and what salsa does today). The store-backed cold form (`InferenceRecord`) arrives
  with `infer` as a blaze-query query, where `Stored::Disk` already exists. No salsa API is
  added for it.
- D7: option (a). blaze-diagnostics reads `placed` from `hir-ty-mir` (already a workspace
  dependency), and nothing moves.
- D8: the previous `Parse` goes in a per-file slot in base-db, written only by `parse`, and the
  edit is the common prefix and suffix computed in `Files::set_text`. This adds no salsa API. A
  reparse must equal a full parse: a property test over random edits checks it, with
  format-parse's tests. Owner: db-expand (dispatch task 24).
- D9: `crates/hir-ty/src/mir.rs` and `mir/` stay lane db's (db-ty-next1 tasks 4 to 7).
- D10: forwarded to diagnostics (request 12).

> applied: noted, nothing to apply (D3 is request 9, D5 request 11, D10 in request 12).

## 11. core (`crates/blaze-graph/`): `pump::wait_until` (db, 20:05)

What: `pub fn blaze_graph::pump::wait_until(ended: &dyn Fn() -> bool)`, the body of the private
`wait` (`pump.rs:509`: spin, then `pool::run_one` below `PUMP_DEPTH`, then `pool::park`).
Where: `crates/blaze-graph/src/pump.rs`.
Why: lock audit item 6. salsa's waits must pump the shared pool as blaze-query's do (#1301).
base-db's `install_plane_hooks` passes it to `salsa::set_wait`. Decision: salsa's cycle
detection moves onto the plane's one wait graph (`pump.rs` `waits()`), and
`Mutex<DependencyGraph>` (`vendor/salsa/src/runtime.rs:35`) goes. That is the speed lane's
change in `vendor/salsa` (speed-salsa-next2), once `wait_until` exists.

> applied: `pub fn blaze_graph::pump::wait_until(ended: &dyn Fn() -> bool)` in `crates/blaze-graph/src/pump.rs` (core is DONE): the old private `wait`'s loop (pump below `PUMP_DEPTH` every 16th turn, spin `SPINS`, `pool::park`); `wait` now calls it. Whoever makes `ended` true must `Thread::unpark` the waiter. Test `wait_until_pumps_on_a_worker_and_parks_elsewhere`.

## 12. Forwards from db-requests (db, 20:05)

The items under "Not lane db: to forward" in
`~/restart-2026-10-28/briefs/v2/db-dispatch/db.md` belong to format, codegen, cargo, the
coordinator, core, diagnostics, solver-sync and safety. Each is listed there with its file,
line and the db task it lands with. Please route each to its lane's request file. Also for
diagnostics, D10: once db-expand's `record_use` query exists,
`blaze-diagnostics/src/lints/names/imports/used.rs` reads it instead of resolving every path
again.

> forwarded to format, codegen, cargo, diagnostics (with D10), solver-sync and safety: each told by tmux, pointing at its list in `db-dispatch/db.md`. core's `wait_until` is applied (request 11). The coordinator's items are `requests'` and held until their inputs exist: `ide-db` `run_all` onto `base_db::run_on_pool` and `install_plane_hooks()` (neither exists yet), `coroutine_witnesses` in `impl_hir_database!` (not declared in `hir-ty/src/db.rs` yet), request 9's entries (no crates yet), `Opened::queries` (`Database::open` still mints its own `ViewId`), the `== 4` asserts (after salsa's 4-byte ids). minicore's `#[must_use]` is in `crates/test-utils/`, the coordinator's.

## From safety-model: model tests for blaze-query

From `safety-model-db` (child of `safety-model`, lane safety; report
`/tmp/blaze-lanes/v2-safety-model-db.md`). For `db-query`, who owns `crates/blaze-query/` and is
still at work, so none of this is written in the crate. Design:
`docs/v2/plan/lanes/safety/model.md` ("The switch", "Model tests"). It is written against
`table.rs` and `lib.rs` as of 2026-10-29 20:xx (`Map<V>`, `Column<T>`, `ValueCell`,
`Database::generation` and `born`, `INSIDE`). `db-query`'s plan (`Map<V, X>`, `Rows<T>`,
`segment.rs`, `memo.rs`) replaces some of these. Apply what still names real code; for a
structure that has been rewritten, keep the test's shape (threads, operations, sequential
model, direct asserts) and point it at the new type. A normal build changes in nothing: same
types, same sizes, no new dependency.

> forwarded to db (db-query).

### 0. A finding first: `add_memo` can lower a key's `born` (`lib.rs:1062`)

`collect` keeps a key whole when `born[key] > started`, its promise being "a key given a memo
since one started is kept by it whole" (`lib.rs:589`). `add_memo` stores the generation it
loaded with a plain `born.insert`, so a giver that loaded the generation *before* the bump can
overwrite a stamp that another giver of the same key set *after* it:

1. giver B loads `generation` = 0;
2. `collect` bumps it (`started` = 0);
3. giver A loads 1, stores `born[k]` = 1, and publishes its memo in `k`'s slot;
4. B stores `born[k]` = 0;
5. `collect`'s `edit_all` reads the slot with A's memo, finds `born[k]` = 0, not young, and,
   since no view reaches the memo yet, removes it.

A's memo was given after the collection began, and the collection drops it. No result is
wrong (a query is deterministic, and the view that asked keeps its `Arc`), but other views run
the query again and the image misses the memo. The model test
`model_collect_keeps_a_memo_given_after_it_bumped_the_generation` (section 5) finds this
schedule (two preemptions). The fix is to let `born` only grow:

`crates/blaze-query/src/lib.rs`, in `Database::add_memo`, before:

```rust
        self.born.insert(task, memo.key, self.generation.load(Ordering::SeqCst));
```

after:

```rust
        let now = self.generation.load(Ordering::SeqCst);
        self.born.update(task, memo.key, |born| born.is_none_or(|born| *born < now).then_some(now));
```

With it the test passes: A's update happens before A's slot publish, which `collect`'s
`edit_all` acquires, so the scan that sees A's memo sees `born[k]` >= 1. A scan that saw the
slot before A published it finds the entry changed under the lock and leaves it, or removes it
first and A's `update` then adds the memo back.

Nothing else in the crate's lock-free code looked wrong. `Map`'s growth copies with relaxed
stores and publishes the table with one release store after them. `update` and `edit_all`
compare entry pointers that cannot be reused while the caller's task holds them. `Column`
publishes with release and reads with acquire. `ValueCell` swaps by AcqRel CAS. The model tests
below check each of these.

### 1. New file `crates/blaze-query/src/sync.rs`

```rust
//! The synchronization types the tables and the database use: `std`'s, or under
//! `--cfg blaze_check` the model checker's, which have `std`'s API
//! (docs/v2/plan/lanes/safety/model.md "The switch"). A normal build sees `std`'s alone.

#[cfg(not(blaze_check))]
#[allow(clippy::disallowed_types)]
pub(crate) use std::{
    sync::{Mutex, MutexGuard, OnceLock, atomic},
    thread_local,
};

#[cfg(blaze_check)]
pub(crate) use blaze_check::{
    sync::{Mutex, MutexGuard, OnceLock, atomic},
    thread_local,
};
```

`Arc` stays `std`'s, as model.md says. No `UnsafeCell` twin: the crate keeps no value in a
cell; values behind an `AtomicPtr` are checked through the atomics and the model tests'
tracked payloads.

### 2. `crates/blaze-query/Cargo.toml`

Add after `[dependencies]` (before `[lints]`):

```toml
[target.'cfg(blaze_check)'.dependencies]
blaze-check.workspace = true
```

The workspace dependency exists (root `Cargo.toml:114`, safety's M1 1); `cfg(blaze_check)` is
in the root `check-cfg` (M1 2). No cycle: `blaze-check` depends only on `blaze-core`,
`rustc-hash`, `libc` and `windows-sys`.

### 3. `crates/blaze-query/src/lib.rs`: the switch

Before:

```rust
mod image;
mod table;
pub mod tree;
```

After:

```rust
mod image;
mod sync;
mod table;
pub mod tree;
```

Before:

```rust
use std::{
    any::{Any, TypeId},
    cell::{Cell, RefCell},
    fmt,
    rc::Rc,
    sync::{Arc, atomic::AtomicU64, atomic::Ordering},
};
```

After:

```rust
use std::{
    any::{Any, TypeId},
    cell::{Cell, RefCell},
    fmt,
    rc::Rc,
    sync::Arc,
};
```

Before:

```rust
use crate::{
    image::ImageMap,
    table::{
```

After:

```rust
use crate::{
    image::ImageMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        thread_local,
    },
    table::{
```

The `thread_local!` of `INSIDE` (`lib.rs:270`) stays as written: the import shadows the
prelude's macro, and `blaze_check::thread_local!` takes the same `const { }` form. Under a model
each model thread gets its own `INSIDE`; with `std`'s, every model thread would share the one
OS thread's, and `inside_a_query` would answer for another thread.

### 4. `crates/blaze-query/src/table.rs`: the switch

Before:

```rust
#[allow(clippy::disallowed_types)]
use std::sync::{Mutex, MutexGuard};
use std::{
    hash::BuildHasherDefault,
    marker::PhantomData,
    ptr,
    sync::{
        Arc, OnceLock, PoisonError,
        atomic::{AtomicPtr, AtomicU32, Ordering},
    },
};
```

After:

```rust
use std::{
    hash::BuildHasherDefault,
    marker::PhantomData,
    ptr,
    sync::{Arc, PoisonError},
};
```

Before:

```rust
use crate::{Edge, QueryKey, QueryKind, Stored};
```

After:

```rust
#[allow(clippy::disallowed_types)]
use crate::sync::{Mutex, MutexGuard};
use crate::{
    Edge, QueryKey, QueryKind, Stored,
    sync::{
        OnceLock,
        atomic::{AtomicPtr, AtomicU32, Ordering},
    },
};
```

Nothing else in `table.rs` changes. `ShortLock::lock`'s `unwrap_or_else(PoisonError::into_inner)`
works on the shim (its `LockResult` is `std`'s). `OnceLock::get_mut` as a path,
`std::array::from_fn(|_| OnceLock::new())`, `get`, `get_or_init`, `AtomicPtr::get_mut`, `swap`
and `compare_exchange` all have the shim's inherent methods. `image.rs` and `tree.rs` have no
synchronization.

### 5. `crates/blaze-query/src/table/tests.rs`

The four threaded tests stay compiled but are ignored in a model build (they spawn 4 to 12
threads and loop thousands of times; `cfg_attr(.., ignore)` rather than `cfg(not(..))` keeps
`AtomicBool` and `freed` used). Add above each of `#[test] fn
readers_see_every_published_entry_whole_while_tables_grow`, `#[test] fn
get_or_insert_keeps_the_first_value`, `#[test] fn an_update_sees_the_entry_it_replaces` and
`#[test] fn a_column_publishes_takes_and_restores_by_index`:

```rust
#[cfg_attr(blaze_check, ignore = "many threads, long loops: slow under the model checker's shims")]
```

Append at the end of the file:

```rust
/// Model tests (docs/v2/plan/lanes/safety/model.md): each structure from two or three threads
/// under every interleaving and every value its loads may read. What a lock, a
/// read-modify-write or a join orders is checked against a sequential model; what a lone
/// acquire load sees is asserted directly, since it may be older than a store that came
/// earlier in the run when nothing orders the two.
#[cfg(blaze_check)]
mod model {
    use std::{
        collections::BTreeMap,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering::Relaxed},
        },
    };

    use blaze_check::{
        cell::UnsafeCell,
        linear::{History, Spec},
        sync::atomic::AtomicU32,
        thread,
    };
    use blaze_core::Mapping;
    use blaze_format::SchemaHash;
    use blaze_ids::{ContentId, Fingerprint};

    use crate::{
        QueryKey, QueryKind, Stored,
        table::{Aligned, COLUMN_FIRST, Column, Edges, Map, MemoData, Value, ValueCell},
    };

    /// Entries that fill a shard's first table: the next insert into the shard grows it.
    const FULL: u8 = 8;

    /// Keys of one shard (their first four bytes pick it), each with a home of its own.
    fn key(n: u8) -> QueryKey {
        let mut input = [0; 32];
        input[8] = n;
        QueryKey { kind: QueryKind(1), input: ContentId(input) }
    }

    #[derive(Default)]
    struct Counts {
        made: AtomicUsize,
        dropped: AtomicUsize,
    }

    impl Counts {
        fn balanced(&self) {
            assert_eq!(
                self.made.load(Relaxed),
                self.dropped.load(Relaxed),
                "every value made is dropped, once"
            );
        }
    }

    /// A value the model tracks: the thread that makes it writes it, and each read is checked
    /// against that write, so a read the structure's release and acquire don't order after it is
    /// a data race.
    struct Tracked {
        value: UnsafeCell<u64>,
        counts: Arc<Counts>,
    }

    // SAFETY: `value` is written once, in `Tracked::new` before the value is shared, and only read
    // after; the model reports any read that write does not happen before.
    unsafe impl Sync for Tracked {}

    impl Tracked {
        fn new(counts: &Arc<Counts>, value: u64) -> Tracked {
            counts.made.fetch_add(1, Relaxed);
            let tracked = Tracked { value: UnsafeCell::new(0), counts: counts.clone() };
            // SAFETY: not shared yet: this is the only access.
            tracked.value.with_mut(|it| unsafe { *it = value });
            tracked
        }

        fn get(&self) -> u64 {
            // SAFETY: written only by `new`, before the value was published.
            self.value.with(|it| unsafe { *it })
        }
    }

    impl Drop for Tracked {
        fn drop(&mut self) {
            self.counts.dropped.fetch_add(1, Relaxed);
        }
    }

    /// A map's sequential model.
    #[derive(Clone, PartialEq, Eq, Hash, Default)]
    struct Entries(BTreeMap<u8, u64>);

    #[derive(Clone, Copy, Debug)]
    enum MapOp {
        Insert(u8, u64),
        GetOrInsert(u8, u64),
        /// Adds to the value (to 0 when there is none); gives the value it replaced.
        Add(u8, u64),
        Get(u8),
    }

    impl Spec for Entries {
        type Op = MapOp;
        type Ret = Option<u64>;

        fn apply(&mut self, op: &MapOp) -> Option<u64> {
            match *op {
                MapOp::Insert(n, value) => {
                    self.0.insert(n, value);
                    Some(value)
                }
                MapOp::GetOrInsert(n, value) => Some(*self.0.entry(n).or_insert(value)),
                MapOp::Add(n, value) => {
                    let old = self.0.get(&n).copied();
                    self.0.insert(n, old.unwrap_or(0) + value);
                    old
                }
                MapOp::Get(n) => self.0.get(&n).copied(),
            }
        }
    }

    struct Writers {
        map: Map<Tracked>,
        history: History<Entries>,
        counts: Arc<Counts>,
    }

    impl Writers {
        fn run(&self, op: MapOp) {
            let task = blaze_reclaim::enter();
            let (map, counts) = (&self.map, &self.counts);
            self.history.record(op, |op| match *op {
                MapOp::Insert(n, value) => Some(map.insert(&task, key(n), Tracked::new(counts, value)).get()),
                MapOp::GetOrInsert(n, value) => {
                    Some(map.get_or_insert_with(&task, key(n), || Tracked::new(counts, value)).get())
                }
                MapOp::Add(n, value) => {
                    let mut replaced = None;
                    map.update(&task, key(n), |old| {
                        replaced = old.map(Tracked::get);
                        Some(Tracked::new(counts, replaced.unwrap_or(0) + value))
                    });
                    replaced
                }
                MapOp::Get(n) => map.get(&task, &key(n)).map(Tracked::get),
            });
        }
    }

    /// Writers of one shard while its table grows: inserts, a racing `get_or_insert_with` (one
    /// value wins, the other is dropped) and an `update` that must see the entry it replaces,
    /// against a map. Each decision is made under the shard's lock, so the history is ordered
    /// by it; the reads after the joins are ordered by them.
    #[test]
    fn model_map_writers_linearize_while_a_shard_grows() {
        blaze_check::model(|| {
            let counts = Arc::new(Counts::default());
            let writers =
                Arc::new(Writers { map: Map::new(), history: History::new(), counts: counts.clone() });
            let mut init = Entries::default();
            {
                let task = blaze_reclaim::enter();
                for n in 0..FULL {
                    writers.map.insert(&task, key(n), Tracked::new(&counts, 100 + u64::from(n)));
                    init.0.insert(n, 100 + u64::from(n));
                }
            }
            let spawn = |ops: [MapOp; 2]| {
                let writers = writers.clone();
                thread::spawn(move || {
                    for op in ops {
                        writers.run(op);
                    }
                })
            };
            let one = spawn([MapOp::Insert(FULL, 1), MapOp::GetOrInsert(FULL + 1, 2)]);
            let two = spawn([MapOp::Add(FULL, 10), MapOp::GetOrInsert(FULL + 1, 3)]);
            writers.run(MapOp::Insert(0, 4));
            one.join().expect("a writer");
            two.join().expect("a writer");
            for n in [0, FULL, FULL + 1] {
                writers.run(MapOp::Get(n));
            }
            writers.history.check(init);
            drop(writers);
            counts.balanced();
        });
    }

    /// A reader of one shard while a writer grows its table and another thread removes an entry:
    /// an entry nobody touches is found in every table (the copy is published only once
    /// complete), every entry found is read whole (the tracked payload), and after the joins the
    /// map holds exactly what was inserted and kept.
    #[test]
    fn model_map_readers_see_entries_whole_while_a_shard_grows_and_loses_one() {
        blaze_check::model(|| {
            let counts = Arc::new(Counts::default());
            let map = Arc::new(Map::<Tracked>::new());
            {
                let task = blaze_reclaim::enter();
                for n in 0..FULL {
                    map.insert(&task, key(n), Tracked::new(&counts, u64::from(n)));
                }
            }
            let writer = {
                let (map, counts) = (map.clone(), counts.clone());
                thread::spawn(move || {
                    let task = blaze_reclaim::enter();
                    for n in [FULL, FULL + 1] {
                        map.insert(&task, key(n), Tracked::new(&counts, u64::from(n)));
                    }
                })
            };
            let reader = {
                let map = map.clone();
                thread::spawn(move || {
                    let task = blaze_reclaim::enter();
                    assert_eq!(
                        map.get(&task, &key(0)).map(Tracked::get),
                        Some(0),
                        "an entry no writer touches is in every table"
                    );
                    for n in [1, FULL, FULL + 1] {
                        let found = map.get(&task, &key(n)).map(Tracked::get);
                        assert!(found.is_none_or(|it| it == u64::from(n)), "key {n} read as {found:?}");
                    }
                })
            };
            {
                let task = blaze_reclaim::enter();
                map.retain(&task, |key, _| key.input.0[8] != 1);
            }
            writer.join().expect("the writer");
            reader.join().expect("the reader");
            let task = blaze_reclaim::enter();
            for n in 0..FULL + 2 {
                let want = (n != 1).then_some(u64::from(n));
                assert_eq!(map.get(&task, &key(n)).map(Tracked::get), want, "key {n}");
            }
            let mut live = 0;
            map.for_each(&task, |_, _| live += 1);
            assert_eq!(live, usize::from(FULL) + 1, "each entry once, the removed one gone");
            drop(task);
            drop(map);
            counts.balanced();
        });
    }

    /// A column's sequential model: its values by index, a taken one `None`.
    #[derive(Clone, PartialEq, Eq, Hash)]
    struct Values(Vec<Option<u64>>);

    #[derive(Clone, Copy, Debug)]
    enum ColumnOp {
        Push(u64),
        Get(u32),
        Take(u32),
    }

    impl Spec for Values {
        type Op = ColumnOp;
        type Ret = Option<u64>;

        fn apply(&mut self, op: &ColumnOp) -> Option<u64> {
            match *op {
                ColumnOp::Push(value) => {
                    self.0.push(Some(value));
                    Some(self.0.len() as u64 - 1)
                }
                ColumnOp::Get(index) => self.0.get(index as usize).copied().flatten(),
                ColumnOp::Take(index) => self.0.get_mut(index as usize).and_then(Option::take),
            }
        }
    }

    /// Values a first chunk holds but one: the next pushes straddle a chunk no thread has made.
    const PUSHED: u32 = COLUMN_FIRST as u32 - 1;

    fn push(column: &Column<Tracked>, history: &History<Values>, counts: &Arc<Counts>, value: u64) -> u32 {
        let index = history
            .record(ColumnOp::Push(value), |_| Some(u64::from(column.push(|_| Tracked::new(counts, value)))));
        u32::try_from(index.expect("a push gives its index")).expect("a u32 index")
    }

    fn get(column: &Column<Tracked>, history: &History<Values>, index: u32) {
        let task = blaze_reclaim::enter();
        history.record(ColumnOp::Get(index), |_| column.get(&task, index).map(Tracked::get));
    }

    /// Three pushes, two of them into a chunk they race to make, each read back by its own
    /// thread or taken, against a vector: indices are dense and distinct and each value is
    /// read whole. A value handed to another thread with a relaxed store is read as nothing or
    /// whole: only the column's release and acquire order it.
    #[test]
    fn model_column_pushes_publish_whole_values_into_a_new_chunk() {
        blaze_check::model(|| {
            let counts = Arc::new(Counts::default());
            let column = Arc::new(Column::<Tracked>::new());
            let history = Arc::new(History::<Values>::new());
            let mut init = Values(Vec::new());
            for n in 0..u64::from(PUSHED) {
                column.push(|_| Tracked::new(&counts, n));
                init.0.push(Some(n));
            }
            let handed = Arc::new(AtomicU32::new(u32::MAX));
            let getter = {
                let (column, history, counts, handed) =
                    (column.clone(), history.clone(), counts.clone(), handed.clone());
                thread::spawn(move || {
                    let index = push(&column, &history, &counts, 1000);
                    handed.store(index, Relaxed);
                    get(&column, &history, index);
                    index
                })
            };
            let taker = {
                let (column, history, counts) = (column.clone(), history.clone(), counts.clone());
                thread::spawn(move || {
                    let index = push(&column, &history, &counts, 2000);
                    let mut taken = None;
                    history.record(ColumnOp::Take(index), |_| {
                        taken = column.take(index);
                        taken.as_deref().map(Tracked::get)
                    });
                    // Held, as the database's limbo holds it, until no reader can have it.
                    (index, taken)
                })
            };
            let mine = push(&column, &history, &counts, 3000);
            let index = handed.load(Relaxed);
            if index != u32::MAX {
                let task = blaze_reclaim::enter();
                let found = column.get(&task, index).map(Tracked::get);
                assert!(found.is_none_or(|it| it == 1000), "index {index} read as {found:?}");
            }
            let theirs = getter.join().expect("the getter");
            let (taken_at, taken) = taker.join().expect("the taker");
            for index in [theirs, taken_at, mine] {
                get(&column, &history, index);
            }
            history.check(init);
            drop(taken);
            drop(column);
            counts.balanced();
        });
    }

    /// Where a memo's value is: a buffer by its version, a mapped file, or nowhere.
    #[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
    enum Held {
        Memory(u8),
        Disk,
        Evicted,
    }

    #[derive(Clone, Copy, Debug)]
    enum CellOp {
        Evict,
        Refill(u8),
        /// Moves the buffer of this version to disk, if it is still the cell's.
        ToDisk(u8),
        Look,
    }

    impl Spec for Held {
        type Op = CellOp;
        type Ret = Option<Held>;

        fn apply(&mut self, op: &CellOp) -> Option<Held> {
            match *op {
                CellOp::Evict => matches!(self, Held::Memory(_)).then(|| std::mem::replace(self, Held::Evicted)),
                CellOp::Refill(version) => {
                    if *self == Held::Evicted {
                        *self = Held::Memory(version);
                    }
                    None
                }
                CellOp::ToDisk(version) => {
                    (*self == Held::Memory(version)).then(|| std::mem::replace(self, Held::Disk))
                }
                CellOp::Look => Some(*self),
            }
        }
    }

    const DISK: ContentId = ContentId([9; 32]);

    /// Where `value` is, checking it is read whole.
    fn where_is(value: &Value) -> Held {
        match value {
            Value::Memory(buffer) => {
                let bytes = buffer.bytes();
                assert!(bytes.len() == 2 && bytes[0] == b'm', "a buffer read whole: {bytes:?}");
                Held::Memory(bytes[1] - b'0')
            }
            Value::Mapped { .. } => {
                assert_eq!(value.bytes(), b"dk", "a mapped value read whole");
                Held::Disk
            }
        }
    }

    /// A value the cell let go of, kept as the database's limbo keeps it.
    fn keep(old: Arc<Value>, gone: &mut Vec<Arc<Value>>) -> Held {
        let held = where_is(&old);
        gone.push(old);
        held
    }

    struct MemoCell {
        memo: MemoData,
        history: History<Held>,
    }

    impl MemoCell {
        fn evict(&self, gone: &mut Vec<Arc<Value>>) {
            let task = blaze_reclaim::enter();
            self.history.record(CellOp::Evict, |_| self.memo.evict(&task).map(|old| keep(old, gone)));
        }

        fn refill(&self, version: u8) {
            self.history.record(CellOp::Refill(version), |_| {
                self.memo.refill(Aligned::copy(&[b'm', b'0' + version]));
                None
            });
        }

        /// A cold write: the buffer this thread sees (a lone load, outside the history), then the
        /// swap that moves it to disk if the cell still holds that buffer.
        fn to_disk(&self, gone: &mut Vec<Arc<Value>>) {
            let task = blaze_reclaim::enter();
            let Some(was) = self.memo.value.memory(&task) else { return };
            let Held::Memory(version) = where_is(&was) else {
                panic!("`ValueCell::memory` gave a mapped value")
            };
            let mapping = Mapping(Arc::new(Aligned::copy(b"dk")));
            self.history.record(CellOp::ToDisk(version), |_| {
                self.memo.to_disk(&was, mapping, 0, 2, DISK).map(|old| keep(old, gone))
            });
        }

        fn look(&self) {
            let task = blaze_reclaim::enter();
            self.history.record(CellOp::Look, |_| {
                let held = self.memo.value.load(&task).map_or(Held::Evicted, where_is);
                let stored = match held {
                    Held::Memory(_) => Stored::Memory,
                    Held::Disk => Stored::Disk(DISK),
                    Held::Evicted => Stored::Evicted,
                };
                assert_eq!(self.memo.stored(&task), stored);
                assert_eq!(self.memo.bytes(&task).is_some(), held != Held::Evicted);
                Some(held)
            });
        }
    }

    /// An eviction, a refill and two cold writes on one memo's value, against where the value
    /// is: each swap is a compare-and-swap, so each decision is made on the latest value. Every
    /// value let go of is let go of once: after the cell is dropped, each is held by its one
    /// keeper alone.
    #[test]
    fn model_value_cell_evicts_refills_and_moves_to_disk_whole() {
        blaze_check::model(|| {
            let cell = Arc::new(MemoCell {
                memo: MemoData {
                    key: key(0),
                    fingerprint: Fingerprint(7),
                    schema: SchemaHash(ContentId([0x57; 32])),
                    producer: ContentId([1; 32]),
                    edges: Edges::Owned(Box::default()),
                    value: ValueCell::new(Some(Value::Memory(Aligned::copy(b"m0")))),
                },
                history: History::<Held>::new(),
            });
            let evicter = {
                let cell = cell.clone();
                thread::spawn(move || {
                    let mut gone = Vec::new();
                    cell.evict(&mut gone);
                    gone
                })
            };
            let refiller = {
                let cell = cell.clone();
                thread::spawn(move || {
                    let mut gone = Vec::new();
                    cell.refill(1);
                    cell.to_disk(&mut gone);
                    gone
                })
            };
            let mut gone = Vec::new();
            cell.to_disk(&mut gone);
            gone.extend(evicter.join().expect("the evicter"));
            gone.extend(refiller.join().expect("the refiller"));
            cell.look();
            cell.history.check(Held::Memory(0));
            drop(cell);
            for value in &gone {
                assert_eq!(Arc::strong_count(value), 1, "the cell gave up its count of a value once");
            }
        });
    }
}
```

### 6. `crates/blaze-query/src/tests.rs`

Append at the end of the file:

```rust
/// Model tests of the database's own lock-free state (docs/v2/plan/lanes/safety/model.md): the
/// collection generation and the thread-local that marks a query running.
#[cfg(blaze_check)]
mod model {
    use std::sync::{Arc, atomic::Ordering};

    use blaze_check::{Builder, thread};
    use blaze_format::SchemaHash;
    use blaze_ids::{ContentId, Fingerprint};

    use crate::{
        Database, Inside, QueryKey, QueryKind, inside_a_query,
        table::{Edges, MemoData, ValueCell},
    };

    const KEY: QueryKey = QueryKey { kind: QueryKind(0x2f00), input: ContentId([3; 32]) };

    fn memo(fingerprint: u128) -> Arc<MemoData> {
        Arc::new(MemoData {
            key: KEY,
            fingerprint: Fingerprint(fingerprint),
            schema: SchemaHash(ContentId([0x57; 32])),
            producer: ContentId([1; 32]),
            edges: Edges::Owned(Box::default()),
            value: ValueCell::new(None),
        })
    }

    /// `Database::add_memo` from two threads racing `Database::collect`: a memo given by a thread
    /// that saw the collection's bump of the generation is young to it and kept; one given
    /// before may go (no view reaches it). A key keeps only memos given, each once.
    #[test]
    fn model_collect_keeps_a_memo_given_after_it_bumped_the_generation() {
        let mut builder = Builder::new();
        // `collect` loads or locks each shard of each table a few times: each is a branch.
        builder.max_branches = builder.max_branches.max(32 * blaze_ids::shard_count() + 10_000);
        builder.check(|| {
            let db = Arc::new(Database::empty());
            let give = |fingerprint: u128| {
                let db = db.clone();
                thread::spawn(move || {
                    let task = blaze_reclaim::enter();
                    let saw_bump = db.generation.load(Ordering::SeqCst) > 0;
                    db.add_memo(&task, &memo(fingerprint));
                    saw_bump
                })
            };
            let givers = [give(1), give(2)];
            db.collect(&[], &mut Vec::new());
            let saw = givers.map(|giver| giver.join().expect("a giver"));
            let task = blaze_reclaim::enter();
            let kept: Vec<u128> = db
                .slot(&task, &KEY)
                .map_or_else(Vec::new, |slot| slot.memos.iter().map(|it| it.fingerprint.0).collect());
            assert!(kept.iter().all(|it| *it == 1 || *it == 2), "only memos given are kept: {kept:?}");
            for (fingerprint, saw_bump) in [1, 2].into_iter().zip(saw) {
                let count = kept.iter().filter(|it| **it == fingerprint).count();
                assert!(count <= 1, "memo {fingerprint} is kept {count} times: {kept:?}");
                assert!(
                    !saw_bump || count == 1,
                    "memo {fingerprint}, given after the collection bumped the generation, was dropped: \
                     {kept:?}"
                );
            }
        });
    }

    /// `INSIDE` is each thread's own: one thread's running query is never another's, at any
    /// interleaving of their marks.
    #[test]
    fn model_inside_a_query_is_per_thread() {
        blaze_check::model(|| {
            let other = thread::spawn(|| {
                assert!(!inside_a_query(), "another thread's query is not this one's");
                thread::yield_now();
                let inside = Inside::enter();
                thread::yield_now();
                assert!(inside_a_query());
                drop(inside);
                assert!(!inside_a_query());
            });
            let inside = Inside::enter();
            thread::yield_now();
            assert!(inside_a_query(), "this thread's mark is its own");
            drop(inside);
            thread::yield_now();
            assert!(!inside_a_query());
            other.join().expect("the other thread");
        });
    }
}
```

Run: `RUSTFLAGS="--cfg blaze_check" blaze test -p blaze-query --release model_`. The tests
use only `std` and blaze-check's API as it is in `crates/blaze-check/src` today (`model`,
`Builder::max_branches`, `thread::spawn`/`yield_now`, `cell::UnsafeCell::with`/`with_mut`,
`linear::{History, Spec}`, `sync::atomic::AtomicU32`); blaze-check has no `thread::scope`
yet, so each test shares through `Arc`. `blaze-reclaim` has no switch yet (safety-model-store's
crate): its `std` thread-local is then shared by the model's threads on one OS thread, which
only delays frees, and `Limbo::collect` never runs in these tests (`COLLECT_AT` is 16). They
pass with or without it.

## From speed-backend-sanitize (2026-10-29): `#[sanitize(.. = "off")]` for codegen (`crates/hir-def/src/attrs.rs`)

What: a query giving a function's effective set of sanitizers turned off, as rustc's
`sanitizer_settings_for` (`compiler/rustc_codegen_ssa/src/codegen_attrs.rs` at the pinned
nightly) computes it:

```rust
/// The sanitizers `#[sanitize(address = "off", ..)]` turns off for `def`, from its own
/// attribute and those of its parents (impl, module, crate), the nearest winning:
/// `blaze_codegen::settings::Sanitizers` bits ADDRESS, THREAD, MEMORY.
pub fn disabled_sanitizers(db: &dyn DefDatabase, def: FunctionId) -> u8;
```

Rules (rustc's): start from the parent's set (the crate root has none); each `key = "off"` of
this item's `#[sanitize(..)]` sets the bit, each `key = "on"` clears it; the keys are `address`
and `kernel_address` (ADDRESS), `thread`, `memory`; the attribute is accepted on fns, closures,
methods, impls, modules, the crate and statics, under feature `sanitize`. `#[no_sanitize]` is
gone from the pinned nightly; refuse it as rustc does. Other keys (`cfi`, `kcfi`, `hwaddress`,
`memtag`, `shadow_call_stack`, `safestack`, `realtime`) are parsed and kept out of this set,
since blaze does not instrument them.
Why: S2 of `v2/requests/codegen.md`: every function but those turned off gets
`sanitize_address`/`sanitize_thread`/`sanitize_memory`; the fuzz engine's callbacks are
`#[sanitize(address = "off")]` (`fuzz/src/raw.rs`). The adapter reads this into
`Function::unsanitized` (codegen.md "From speed-backend-sanitize" Z3.2) and items hashes it
(`v2/requests/items.md`, same date).

> written by speed-backend-sanitize (2026-10-29); told db by tmux.
> forwarded to db.

> applied by db-apply-def (21:14): it was not in `hir-def/src/attrs.rs` (only its two callers, `blaze-items/src/form.rs` and `hir-ty-codegen/v2/backend.rs`, were). Written there: `pub fn disabled_sanitizers(db: &Db, def: FunctionId) -> u8` (`&Db`, not `&dyn DefDatabase`), bits in `attrs::sanitizer::{ADDRESS, THREAD, MEMORY}`; gated by a new `AttrFlags::HAS_SANITIZE` (`#[sanitize(..)]`, one segment; `Word::sanitize`); the chain is rustc's `opt_local_parent`: impl / trait / extern block, module, crate root, and for an item in a body every fn, closure, impl, trait, const and static its block is in (walked by syntax, through the macro calls the block came from, up to the module holding the block), via tracked `module_disabled_sanitizers` / `block_disabled_sanitizers`. An item's first `#[sanitize]` counts; `on` keys are cleared before `off` keys are set. Tests: `attrs/tests.rs` `mod sanitize` and the gating-flag tables. Refusing `#[no_sanitize]` is the diagnostics lane's (its feature table lists it Removed).

## frontend: what frontend.md items 7-12 still need in db's paths (lane frontend, 2026-10-29 19:10)

Lane `frontend` checks each item of `docs/v2/plan/design/frontend.md`. These are db's (items 9,
10, 12 with core; 11; 8 with everyone; 7's signatures). Each is what the code shows today;
answer with `>` lines (applied / planned by whom / declined and why).

1. **Item 9, cancellation by unwinding.** `vendor/salsa/src/cancelled.rs:33-38`,
   `zalsa.rs:311-314`, `function/fetch.rs:21` still unwind with `salsa::Cancelled`, and the v2
   daemon relies on it (`crates/load-cargo/src/v2/db.rs:17-23`: a write sets the flag, readers
   unwind, the worker maps it to `Failed::Again`). The doc: the plane stops scheduling a
   cancelled request's tasks, and a running task sees the flag at its next query call and
   returns. Asked: the cancellation check at a query call returns (`Err(Cancelled)` up the
   query stack, as `blaze_query::Cx::get` does, `crates/blaze-query/src/lib.rs:346-357`) rather
   than `panic::resume_unwind`; the hir queries propagate it; no `catch_unwind` on the build's
   path.
2. **Item 10, per-worker handles.** `load-cargo/src/v2/db.rs:15-35` (`Db: Clone`, `Snapshot`
   per worker, boxed into `hir_ty_codegen::v2::Databases`). Your Done when says "no salsa
   per-thread handles". Asked: the query stack per task (not per handle), so workers share
   `&Db`; then cargo-daemon drops `snapshot` and codegen's `Databases` takes `&dyn HirDatabase`
   (frontend sent both their parts: `v2/requests/cargo.md`, `v2/requests/codegen.md`).
3. **Item 12, LRU on the build's path.** `#[salsa::tracked(lru = ..)]` on
   `expr_store/body.rs:111,161` and `attrs.rs:1686` (and others): an evicted memo must keep
   its fingerprint and edges so a re-execution that gives the same fingerprint still cuts off
   (the salsa fork has edge fingerprints, `zalsa_local.rs:538-603`,
   `function/maybe_changed_after.rs:320-322`: please confirm eviction keeps them for every
   `lru` query, with a test that evicts and edits elsewhere and counts dependents that rerun:
   zero).
4. **Item 11, interned values never freed.** `vendor/salsa/src/interned.rs:99-115` reuses slots
   by CLOCK within revisions; nothing collects by reachability from live views
   (`blaze_query::Database::collect`, `lib.rs:761-798`, covers its own memos only). Asked: salsa
   interned values collected with the image's GC by reachability from live views, keeping IDs
   stable for live values (no renumbering; restore.md: IDs are indexes into mapped tables).
   `frontend-image` does `crates/intern` and the image's interner section and will send you the
   API.
5. **Item 8, `Arc` per node.** hir-def/hir-ty/hir-expand/base-db still hold `triomphe::Arc` per
   value (e.g. `Body::of -> Arc<Body>`, `signatures.rs`, `base-db/src/lib.rs:50`). Asked:
   values returned by reference from their memo (`returns(ref)`), not `Arc` clones, on the
   build's hot paths; list in your report which remain and why.
6. **Item 7, signatures' source maps.** `*Signature::of` goes through `with_source_map`
   (`crates/hir-def/src/signatures.rs:206-243`), so every signature lowering builds a map.
   Asked: `of` lowers without the map; `with_source_map` lowers with it on demand; same
   signature (test over every signature fixture). The body side is asked of
   speed-hir-store-srcmap (`v2/requests/speed-hir.md`).
7. **Items 1, 5, 6, 14 and 4**: `frontend-nameres`, `-deps`, `-text`, `-tokens` will send exact
   hook edits for `nameres`, `base-db`'s parse and `FileText`, and `hir-expand`'s expansion,
   under `## frontend-<part>:` headings here.

> forwarded to db.

> db (db-next3, 21:15), item by item:
> 1. **Declined as written** (Andrew's decision, 21:10). The unwind stays, but only to carry a
>    stop from a query call up to the task it runs in: it is caught once per plane task
>    (`base_db::Db::run_all`, `base_db::Shared`), never per query. The plane never schedules a
>    cancelled request's tasks, and a running task stops at its next query call. Making
>    `Result<_, Cancelled>` the return of every tracked query in hir-def, hir-ty and hir would
>    put a branch on every hit and `?` on thousands of call sites, for an event that comes once
>    per edit made during a build. The coordinator is asked to reword item 9 to this (request
>    14 below). New test: a reader stopped mid-query keeps the memos it finished and stores none
>    for the query it was in (`crates/base-db/src/db/shared/tests.rs`).
> 2. **Done.** `base_db::Db` is one concrete `Sync` database with per-task query stacks
>    (db-salsa part 2, `salsa::set_task_depth`). Scoped work borrows `&Db`, and a task that
>    outlives a borrow holds a `base_db::Reader`, whose owner writes through `Shared::write`
>    (`&self` `stop_readers`, then waiting for the readers). `load-cargo/src/v2/db.rs` and its
>    `Snapshot` are gone. Cargo's and codegen's parts are theirs.
> 3. **Done.** salsa keeps an evicted memo's fingerprint and edges for every `lru` query.
>    `vendor/salsa/tests/lru_fingerprint.rs` evicts, edits elsewhere and checks the logs:
>    the recomputed value is backdated and nothing that read it reruns. `evict_values.rs` and
>    `lru_cycle.rs` cover the eviction API and cycles. In blaze-query, `evict` keeps cutoff
>    (its tests).
> 4. Interned values are never freed now (db-salsa part 2). GC by reachability with stable
>    IDs is `## frontend-image` §6 below, which `vendor/salsa`'s current holder (speed-salsa)
>    takes after S5.
> 5. A hit reads a signature or body by reference (`returns(deref)` / `returns(ref)`), so a
>    hit costs no count. The `Arc`s left are one per memo, not one per node: `*Signature::of`
>    (shared between `of` and `with_source_map`), `Body::of`, and `base-db`'s inputs. Moving
>    them into the memo is with db-expand (hir-def) and db-ty (hir-ty).
> 6. **Open, with db-expand-next2.** `of` still lowers through `relative`, which builds the
>    map. It goes to db-expand-next2's dispatch as written, with the test over every signature
>    fixture.
> frontend-hooks (2026-10-29): not applied: design asks, db's (Andrew's decision 2026-10-29 20:45).
> The later `## frontend-*` sections that give exact edits are handled each in its own place.

## frontend-deps: declare `hir_def::deps` (dependency images, frontend.md item 5; 2026-10-29)

`crates/hir-def/src/deps.rs` and `deps/{key,image,serve,tests}.rs` are new (lane
`frontend-deps`): a dependency crate's def map, lang items and signatures kept as one image keyed
by every input, served from the store or global cache (`deps::serve`) and written on a miss
(`deps::store`). `deps/signatures.rs` comes from `frontend-deps-sigs`, which sends its own hooks
into `signatures.rs`. Nothing calls the module until cargo's driver does (request in
`v2/requests/cargo.md`). The module needs declaring:

- File: `crates/hir-def/src/lib.rs`
- Old:
  ```rust
  pub mod nameres;

  pub mod src;
  ```
- New:
  ```rust
  pub mod nameres;

  pub mod deps;

  pub mod src;
  ```
- Why: without it the module is not compiled. It reads `nameres::frozen`'s pub API only
  (`encode`, `encode_lang_items`, `set`, `set_lang_items`, `unreadable`, `ENCODING`, `Mode`,
  `Frozen`), so no other db file changes.

Already done, thanks: `EditionedFileId::parse` parses library source roots as an outline
(`crates/base-db/src/editioned_file_id.rs:25-34`), which item 5 needed.

> forwarded to db (window `db-next2`, told by tmux): `crates/hir-def/` is its.
> applied by frontend-hooks (Andrew's decision 2026-10-29 20:45): `pub mod deps;` between
> `pub mod nameres;` and `pub mod src;` in `crates/hir-def/src/lib.rs`. Checked `deps/` against
> `frozen.rs` as it is now (`set`/`set_lang_items` take `&mut Db`, `Frozen.bytes: Arc<[u8]>` with
> `From<PartBytes>`). `deps/signatures/table.rs` also needs frontend-deps-sigs §1 (frozen's
> `Enc`/`Dec` id methods `pub(crate)`), which is left for db-expand-names-next2 (below).

## frontend-deps-reparse: parse a file from its previous parse (frontend.md item 14; 2026-10-29)

`EditionedFileId::parse` parses the whole text on every edit; it never sees the previous tree.
`syntax::SourceFile::parse_file` (new, `crates/syntax/src/lib.rs`, lane `frontend-deps-reparse`)
keeps the last parse of each recently parsed file (128 direct-mapped slots,
`crates/syntax/src/parsing/kept.rs`) and makes the next parse of that file with
`Parse::reparse_to(text, edition)`: the bytes between the longest common prefix and suffix of the
previous tree's text and the new text are the edit, and only the smallest token, lazy body, block or
top-level item holding it is parsed again and spliced (`parsing/reparsing.rs`), sharing the new
`FileText` `Arc<str>`. The edit is computed from the two texts, so nothing about the edit has to be
stored in base-db or trusted: several VFS edits between two parses are one covering edit, and a slot
another file took is a whole parse. This is not a fallback: the reparse and a whole parse give the
same tree (`reparse_random_edits`, `reparse_to_random_edits` and `kept::tests` compare them, in full
and as outlines), so which one runs changes the time only. The outline/full choice stays the query's
(`is_library`); a kept parse of the other kind is not used.

- File: `crates/base-db/src/editioned_file_id.rs`
- Old:
  ```rust
    /// The file's tree, sharing its text. A file of a library source root (a dependency's) is an
    /// outline, whose `fn` bodies are parsed when first walked into (#1294); every other file is
    /// parsed in full. Decided by the source root, so a file two crates read gets one answer.
    #[salsa::tracked(lru = 128, returns(ref))]
    pub fn parse(self, db: &dyn SourceDatabase) -> syntax::Parse<ast::SourceFile> {
        let _p = tracing::info_span!("parse", ?self).entered();
        let (file_id, edition) = self.unpack(db);
        let text = db.file_text(file_id).text(db).clone();
        match self.is_library(db) {
            true => ast::SourceFile::parse_outline(text, edition),
            false => ast::SourceFile::parse_shared(text, edition),
        }
    }
  ```
- New:
  ```rust
    /// The file's tree, sharing its text. A file of a library source root (a dependency's) is an
    /// outline, whose `fn` bodies are parsed when first walked into (#1294); every other file is
    /// parsed in full. Decided by the source root, so a file two crates read gets one answer.
    /// After an edit, only what the edit touches is parsed again, from the file's last parse
    /// ([`ast::SourceFile::parse_file`]); the tree is the one a whole parse makes.
    #[salsa::tracked(lru = 128, returns(ref))]
    pub fn parse(self, db: &dyn SourceDatabase) -> syntax::Parse<ast::SourceFile> {
        let _p = tracing::info_span!("parse", ?self).entered();
        let (file_id, edition) = self.unpack(db);
        let text = db.file_text(file_id).text(db).clone();
        let key = self.span_file_id(db).as_u32();
        ast::SourceFile::parse_file(key, text, edition, self.is_library(db))
    }
  ```
- Why: frontend.md item 14 ("v2 reparses only the items an edit touches"). The per-item item
  tree (`crates/hir-def/src/item_tree/`, same lane) then re-lowers only the items whose syntax
  changed. No new dependency: base-db already depends on `syntax`.

> forwarded to db (window `db-next2`, told by tmux).
> frontend-hooks (2026-10-29): not applied, superseded. db did item 14 its own way (decision D8
> above): `crates/base-db/src/reparse.rs`, a `Reparse` set with each `FileText` that carries the
> edit, and `EditionedFileId::parse` is `input.reparse_field(db).parse(..)`. Replacing that with
> `parse_file` would undo db's design. `syntax::SourceFile::parse_file` and
> `syntax/src/parsing/kept.rs` now have no caller outside syntax; whether they go is frontend's call.

## frontend-text: one lazy line index per file text (frontend.md item 6; 2026-10-29)

`span::LineIndex` is new (lane `frontend-text`, `crates/span/src/lines.rs`): a bitmap of the
text's `\n` bytes made with SIMD (AVX-512BW/AVX2/SSE2/NEON, chosen at run time, every path
checked against the scalar one) and a rank table per 512 bytes; `place(offset)` gives a
`span::Place` whose `line()`, `char_column(text)`, `display_column(text)` and
`byte_column(text)` are rustc's (`lookup_char_pos`: byte order mark dropped, `\r\n` read as `\n`,
a last `\n` starting no line; tested against a model of `rustc_span::SourceFile` at every
offset). Today two salsa queries scan each text for line starts (`hir-expand`'s `line_starts`,
`hir-ty-mir`'s), plus load-cargo's `line_index` crate. One query in base-db replaces them.

> forwarded to db (window `db-next2`, told by tmux).
> applied by db-expand-macros: section 2. `line_and_column` (`crates/hir-expand/src/builtin/fn_macro.rs`) reads `base_db::line_index(db, text).place(..)` with `line()` and `char_column(..)`, its doc comment adds the `\r\n` rule, `line_starts` is gone from `fn_macro.rs` and from `builtin.rs`'s re-export, and `FileText` is dropped from the imports. Test `line_and_column_after_a_byte_order_mark_and_crlf_lines` (`crates/hir-def/src/macro_expansion_tests/builtin_fn_macro.rs`).

### 1. `crates/base-db/src/lib.rs`: the query

After `impl FileText { ... }` (the block ending with `pub fn content<Db ...>`), add:

```rust
/// Where the lines of a file's text are ([`span::LineIndex`]): made the first time a
/// diagnostic, `line!`/`column!`, a caller location, a proc macro's `Span::line` or debug info
/// asks for a line or a column of the text, once per text, and never on load.
#[salsa::tracked(returns(ref))]
pub fn line_index(db: &dyn SourceDatabase, text: FileText) -> span::LineIndex {
    span::LineIndex::new(text.text(db))
}
```

(`span` is already a base-db dependency; `LineIndex` is `Clone + Eq + Debug + 'static`.)

> applied by frontend-db-base: in `crates/base-db/src/lib.rs` after `impl FileText`, as
> `line_index(db: &Db, text: FileText)` (the database is concrete now). Sections 2 on are not.

### 2. `crates/hir-expand/src/builtin/fn_macro.rs`: `line_and_column` on it

Old (from `pub fn line_and_column` to the end of `line_starts`):

```rust
pub fn line_and_column(db: &dyn SourceDatabase, range: FileRange) -> Option<(u32, u32)> {
    let text = db.file_text(range.file_id.file_id(db));
    let starts = line_starts(db, text);
    let at = u32::from(range.range.start());
    let line = starts.partition_point(|&start| start <= at);
    let line_start = *starts.get(line.checked_sub(1)?)? as usize;
    let mut written = text.text(db).get(line_start..at as usize)?;
    if line == 1 {
        written = written.strip_prefix('\u{feff}').unwrap_or(written);
    }
    Some((line as u32, written.chars().count() as u32 + 1))
}

/// Where each line of a file begins, the first at 0.
#[salsa::tracked(returns(deref))]
pub fn line_starts(db: &dyn SourceDatabase, text: FileText) -> Box<[u32]> {
    let text = text.text(db).as_bytes();
    let mut starts = Vec::with_capacity(text.len() / 32 + 1);
    starts.push(0);
    starts.extend(
        text.iter().enumerate().filter(|&(_, &byte)| byte == b'\n').map(|(at, _)| at as u32 + 1),
    );
    starts.into_boxed_slice()
}
```

New:

```rust
pub fn line_and_column(db: &dyn SourceDatabase, range: FileRange) -> Option<(u32, u32)> {
    let text = db.file_text(range.file_id.file_id(db));
    let place = base_db::line_index(db, text).place(range.range.start())?;
    Some((place.line(), place.char_column(text.text(db))?))
}
```

and in `crates/hir-expand/src/builtin.rs` drop `line_starts` from the `fn_macro::{...}`
re-export (nothing else uses it: `rg line_starts crates` finds only these and hir-ty-mir's
own). If `FileText` is then unused in `fn_macro.rs`, drop it from its imports. What changes:
the doc comment above says "a line ends at each `'\n'`, a column is a code point, and a byte
order mark that starts the file is no column"; add "and the `\r` of a `\r\n` is no column of
its `\n`". `column!()` at the `\n` of a `\r\n` cannot happen (a macro call is not there), so
`line!`/`column!` give what they gave on every program; the test
`places_match_the_old_line_starts` in `crates/span/src/lines/tests.rs` is the differential
against the code removed here.

### 3. File text shares the VFS's bytes (zero copy)

`blaze_vfs::text_of(Arc<[u8]>) -> Result<Arc<str>, Utf8Error>` is new
(`crates/blaze-vfs/src/text.rs`; module declared by request to `frontend-journal`): it
checks the bytes are UTF-8 and casts the tree's own `Arc` to `Arc<str>`, so a `FileText`
holds the VFS's allocation instead of `Arc::from(&str)`'s copy of every file. load-cargo
already holds that `Arc` (`files::Loaded::bytes`, from `Reader::file`); `ViewFile` has to carry
it instead of a borrowed `&[u8]`.

`crates/base-db/src/view.rs`:

- `ViewFile`:
  ```rust
  /// One file of a view, as blaze-vfs reads it.
  #[derive(Clone, Copy, Debug)]
  pub struct ViewFile<'a> {
      /// Relative to the view's root and `/`-separated, as blaze-vfs names it.
      pub path: &'a str,
      /// Its blaze-vfs content ID.
      pub id: ContentId,
      /// Its bytes, the tree's own allocation: a file with text keeps it as its text
      /// ([`blaze_vfs::text_of`]), with no copy.
      pub bytes: &'a std::sync::Arc<[u8]>,
  }
  ```
  (was `pub file: blaze_vfs::File<'a>`; nothing read its `mode`).
- `plan_view`: `file.file.id` -> `file.id` (three places: the `view_ids` check, `given:`, the
  mismatch compare), and
  `plan.writes.iter().map(|it| view.files[it.at].file.bytes).collect()` ->
  `plan.writes.iter().map(|it| &view.files[it.at].bytes[..]).collect()`.
- `write_view`:
  ```rust
  let texts: Vec<Option<std::sync::Arc<str>>> = plan
      .writes
      .iter()
      .map(|it| blaze_vfs::text_of(std::sync::Arc::clone(view.files[it.at].bytes)).ok())
      .collect();
  ```
  `FileText::content_bytes(&view.files[write.at].file.id, ...)` -> `(&view.files[write.at].id,
  ...)`; `writer.view_ids.insert(write.id, view.files[write.at].file.id)` -> `.id`; and since
  `text` is no longer `Copy`, `let unchanged = match (text, self.text_input(write.id))` ->
  `match (&text, self.text_input(write.id))`. The `write_text` call stays
  `text.map(|text| (text, content))`, now an `Option<(Arc<str>, ContentId)>`.

`crates/base-db/src/files.rs`:

- `write_text`'s parameter `text: Option<(&str, ContentId)>` -> `text:
  Option<(std::sync::Arc<str>, ContentId)>`, and its two `std::sync::Arc::from(text)` (the
  `set_text_field` of an existing input and `FileText::builder(...)`) -> `text`.
- `set_text` (text set from outside a view, a `&str`): `Some((text, content))` ->
  `Some((std::sync::Arc::from(text), content))`.

`crates/base-db/src/tests.rs`: `vfs_file` and the three `ViewFile { path, file: vfs_file(bytes)
}` sites (`set_view` ~L376, `a_content_id_that_is_not_its_bytes_is_refused` ~L470, the view
property test ~L697) build the files' `Arc<[u8]>`s first (`let bytes: Vec<Arc<[u8]>> = ...`) and
make `ViewFile { path, id: ContentId::of(blaze_vfs::FILE_DOMAIN, bytes), bytes }`; the refused
test gives a wrong `id` as it gives a wrong `file.id` today. Worth one more assert where a view
is set: `std::ptr::eq(db.file_text(id).text(&db).as_ptr(), bytes.as_ptr())`: the text is the
view's allocation.

The caller to change with it is load-cargo's `set_files` (request in `v2/requests/cargo.md`,
same heading).

Why: frontend.md item 6 ("v2 maps files from the VFS, read in place"): today every file's text
is copied once on load and kept, as much memory again as the VFS holds.

> frontend-hooks (2026-10-29, Andrew's decision 2026-10-29 20:45): §2 left for
> db-expand-macros-next2 (`crates/hir-expand/` is its, running; `line_starts` is still in
> `builtin/fn_macro.rs`); told by tmux. §3 applied in the form the two later frontend-text sections
> give it (the second replaces the amendment): see "the parse tree's source is a `Text`" below.

## speed-hir-store: one counting allocator for hir-def's tests, from `blaze-alloc-count`

What: hir-def's allocation-budget tests share one `#[global_allocator]`. A binary can only have
one, and today it is a private copy in `nameres/tests/paths.rs`.
1. Root `Cargo.toml`, `[workspace.dependencies]`, beside the other `blaze-*` path deps:
   `blaze-alloc-count = { path = "crates/blaze-alloc-count" }`, the form of
   `blaze-ids` at ~L123.
2. `crates/hir-def/Cargo.toml`, under `[dev-dependencies]`: `blaze-alloc-count.workspace = true`.
3. `crates/hir-def/src/lib.rs`, at the end:
   ```rust
   #[cfg(test)]
   #[global_allocator]
   static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;
   ```
4. `crates/hir-def/src/nameres/tests/paths.rs`:
   - Delete `#[global_allocator] static ALLOCATOR: alloc_count::Counting = alloc_count::Counting;`
     (~L745) and the whole `mod alloc_count { .. }` (~L748-805).
   - At its one use (~L740): `let (res, allocations) = alloc_count::count(resolve);` becomes
     `let (res, used) = blaze_alloc_count::measure(resolve);`, and
     `assert_eq!(allocations, 0);` becomes `assert_eq!(used.allocations, 0);`.
Why: the coordinator's rule 3 (allocation budgets with `blaze-alloc-count`).
`expr_store/tests/alloc.rs` (new, speed-hir-store) uses `blaze_alloc_count::assert_at_most` for
store reads, `Body::of`/`ExprScopes::of` memo hits and scope lookups, all with a budget of 0. It
doesn't link until 1-3 land, and with 3 but not 4 the binary has two global allocators.

> applied by db-apply-def (21:12): item 2 (`blaze-alloc-count.workspace = true` under hir-def's `[dev-dependencies]`; manifest lint clean) and item 3 (the `#[global_allocator]` at the end of `lib.rs`) were missing and are in; item 4 was already in (`nameres/tests/paths.rs` uses `blaze_alloc_count::assert_at_most`, no private allocator); hir-def has one `#[global_allocator]`.

> applied (1) by requests: `blaze-alloc-count` is in the root `[workspace.dependencies]`. 2-4 (`hir-def`) forwarded to db (window `db-next2`, told by tmux).

## frontend-image: the daemon image on `blaze-image`, stable symbol IDs and the interner GC (restore.md items 1–3, frontend.md item 11; to db-query and db, 2026-10-29)

New and ready (lane `frontend-image`): `crates/blaze-image` (one image file: a spine checked by
one BLAKE3 at open, sections on 64 KiB group boundaries, one hardware CRC32C a group checked on
first touch, `Corrupt { section, group, cause }` naming what failed), `intern`'s symbol table as
a mapped table with stable `u32` IDs and GC, `intern::Mapped<T>` for stable-ID interned slices,
and `blaze_format::Sym`. The edits below are yours to make; nothing in `crates/blaze-query`,
`vendor/salsa`, `hir-*` was touched.

> forwarded to db and db-query (windows `db-next2`, `db-query-next2`, told by tmux).

### 1. `crates/blaze-query/Cargo.toml`

```toml
blaze-image.workspace = true
intern.workspace = true
```

### 2. `crates/blaze-query/src/image.rs`: sections instead of one hand-laid file

- The image's owner is `blaze_core::Mapping` (`blaze-image` has `unsafe impl Stable for
  Mapping`): the daemon, the db and the VFS share one `Arc<Image<Mapping>>`, opened with
  `Image::open(mapping)`. A section kept as a `Mapping` is `image.owner().clone()` plus
  `image.whole_range(&section)?`.

- `write(task, store, entries, inputs) -> Vec<u8>` becomes
  `write(task, store, entries, inputs, builder: &mut blaze_image::Builder)`: the header and
  root go (the spine is both); each part is a section with its own schema
  (`blaze_format::schema("blaze v2 db memos 2: ...")`, one string a part, from today's
  `LAYOUT` split):
  `builder.section(SectionId::DB_MEMOS, ..)`, `DB_EDGES`, `DB_TABLE`, `DB_INPUTS`, `DB_BLOB`.
  Offsets in records (value at, key at, path at) become offsets into `DB_BLOB`; a value's
  16-byte alignment is `out.align(16)` before `out.bytes(value)`.
- `ImageMap { mapping, .. }` becomes
  `ImageMap { image: Arc<Image<Mapping>>, memos: Section, edges: Section, table: Section,
  inputs: Section, blob: Section }`. `open(image)` is five `image.section(id, schema)?` (a
  missing or other-schema section is `Refused`, the database starts empty: today's
  `DbError::Image`). Nothing else is read at open: no CRC is checked until a group is touched.
- Every read goes through `image.range(&section, at, len)` / `image.slice::<EdgeRecord>(..)`,
  which return `Result<_, Corrupt>`. The six panics "the daemon image is corrupt" (lines
  333, 349, 365, 378, 407, 413 today) split in two:
  - bytes that fail their CRC: `Err(Corrupt)` up to the caller. `memo(index)` and `find(key)`
    return `Result<Option<..>, Corrupt>`; `Database::slot` (lib.rs:1188) on `Err(corrupt)`
    treats the key as having no image memo (so it is recomputed, and its dependents verify
    against the new fingerprint as after any miss), and records `corrupt` once in the
    database's events so the daemon reports the section and group;
  - bytes that pass their CRC but are not what the writer writes (a table slot past the memo
    count, edges past the edge section): a bug in `write`, a panic naming it, as today.
  `inputs()` at `from_image_with` touches only `DB_INPUTS` and `DB_BLOB`'s paths: a `Corrupt`
  there starts the database empty with the error, since inputs are what every memo is checked
  against.
- Test (yours, beside `image.rs`'s): flip one byte in each group of each db section of a
  written image; each flipped group is reported once by section and group and the keys whose
  memos lived there are recomputed, every other key is used from the image.

### 3. `Database::from_image_with` (lib.rs:758): symbols first

Values in the image may name symbol IDs (from 5 on). Before using any section:

```rust
if intern::restored_from() != Some(image.hash()) {
    return Err(DbError::Symbols);  // "the symbol table was not restored from this image"
}
```

The daemon (cargo's request) calls `intern::restore_symbols(image.clone(), &jobs)` before anything
interns, then opens the database with the same `Arc<Image<_>>`. `restore_symbols` checks the
symbol section whole (eagerly: every value depends on it) and returns `RestoreError`
(`Refused`, `Corrupt`, `Invalid`, `Started`); on any of them nothing in the image that names
symbols may be used, so the database starts empty.

### 4. `Database::write_image` (lib.rs:977): one image, GC marks

```rust
pub fn write_image(&self, builder: &mut blaze_image::Builder, live: &mut intern::Live)
```

writes its five sections into `builder` and, for each memo's value bytes, calls the kind's
`<Q::Value as Persist>::symbols(bytes, &mut ids)` (keep `fn(&[u8], &mut Vec<u32>) ->
Result<(), FormatError>` in the kind table at `register`) and `live.mark_id` each. The
daemon adds its sections (VFS, plane history), marks the symbols it holds, then calls
`intern::write_symbols(&live, &mut builder)` last and writes `builder.finish()?` with one
`Effect::Write` (temp, fsync, rename, directory fsync: already `fs::write_atomically`).
`Persist::symbols` is new in `blaze-format` with a default that names none, right for every
value today (they keep names as text).

### 5. Persisted names as `Sym`, not text (hir-def, hir-ty persist)

`crates/hir-def/src/expr_store/persist.rs` and `crates/hir-ty/src/next_solver/persist.rs`
write a `Name`/`Symbol` as its text and re-intern on read. Asked: write
`blaze_format::Sym(symbol.id())` and read with `Symbol::from_id(sym.0)` (a `None` is a bug:
the GC kept every symbol marked), and override `Persist::symbols` with
`blaze_format::symbols_of::<Root>(buffer, Self::SCHEMA, out)` in the same change. Reading a
name becomes one load instead of a hash and probe, and the value is plain bytes.

### 6. Salsa interned values and item IDs: stable across restarts (frontend.md item 11)

`vendor/salsa/src/interned.rs` reuses slots by CLOCK within revisions. Asked: when a salsa
interned table is persisted into the image, it works as `intern::Mapped<T>` does (and may be
one: `Mapped::<T>::new(tag)` with `SectionId::interned(tag)`, `T` a plain record of the
fields with `Sym`/IDs for names): IDs are indexes into the mapped table; GC at `write_image`
by reachability from live views (`Live::mark_value`); a freed ID is given only to a value
made later, never to a live one; nothing is renumbered. Same for item IDs (`ItemLoc`
interning). `ModPath` and `PathSegments` (`hir-expand`, `Interned<T>` today) move to
`Mapped` when a persisted value names them.

### Proofs on the `frontend-image` side (in the tree)

- `crates/blaze-image/src/tests.rs`: open reads only the spine (bytes touched counted); every
  corrupted group found on first touch and named; a torn write is the old image or refused;
  CRC32C hardware paths equal the scalar reference.
- `crates/intern/src/table/tests.rs`: IDs equal after write-and-reopen over three
  generations; GC never renumbers a live value over eight generations of random marks; a
  freed ID goes only to a new value, lowest first; a restore never accepts a section it cannot
  read (4000 random corruptions); interning from many threads on a restored table.
- `crates/blaze-format/src/tests.rs`: `symbols_of` finds every `Sym` inline, behind `Rel`, in
  slices, in a record holding a slice of itself.

> frontend-hooks (2026-10-29): §1-4 were already applied by db-query (turn 5 of its report;
> `blaze-query/Cargo.toml` has both lines, `from_image_with` checks `intern::restored_from()`,
> `write_image(&self, &mut Builder, &mut intern::Live)` at lib.rs:1184). Not applied: §5 and §6
> are design asks, not exact edits. §5 is in lane format's `expr_store/persist.rs` and db-ty's
> `hir-ty/src/next_solver/persist.rs` (still `intern::Symbol::intern(text)` at L278), and needs
> db's answer on whether those bytes ever leave the daemon image (the store, the global cache),
> where a symbol ID means nothing without the same restored table. §6 is `vendor/salsa`
> (speed-salsa, running). Left for db.

> db (db-next3, 21:15):
> - §5, hir-def: **declined.** `expr_store/persist.rs`'s encoding is the one dependency
>   signature images carry (`hir_def::deps::signatures::table`, `StoreRoot`), and those images
>   are kept by the store and shared between daemons, whose symbol tables differ. A name there
>   stays portable. The fast portable form is one name table per image buffer, read in place
>   and interned once per distinct name when the image is opened. That is format's call in
>   `expr_store/persist.rs`, so it is request 15 at the end of this file.
> - §5, hir-ty: **accepted, with one condition.** `next_solver/persist.rs` already writes
>   definitions as salsa IDs, so its tables have meaning only in the image that wrote them.
>   Names become `Symbol::id()` words, read with `Symbol::from_id` (`None` is an invalid value).
>   The condition: no store key or code-identity hash may read those words. A symbol ID is the
>   same text only within one image, and a key that read it could hit another daemon's entry
>   for different code. The values also have to give their symbols to the image's GC
>   (`Persist::symbols`) in the same change. Sent to db-ty-next2, who checks both before
>   applying.
> - §6: for `vendor/salsa`'s holder (speed-salsa-next3), after S5. The design is as written:
>   `Mapped`'s rules, GC at `write_image` by `Live` marks, no renumbering.

## frontend-text: amendment to "one lazy line index per file text", section 3: `Bytes` and `Text` (2026-10-29)

frontend-journal-tree is changing blaze-vfs's reads to open the tree in place from the image:
`Reader::file` now gives `Result<Option<(ContentId, Mode, blaze_vfs::Bytes)>, Corrupt>`, where a
`Bytes` is a heap file's `Arc<[u8]>` or a range of the mapped image. `text_of` follows:
`blaze_vfs::text_of(Bytes) -> Result<blaze_vfs::Text, Utf8Error>`. A `Text` is the `Bytes`,
checked UTF-8 once and `Deref<Target = str>`; `Text::shared() -> Option<Arc<str>>` is the same
allocation for a heap file and `None` for an image range. Section 3 changes as follows (sections 1
and 2 are unchanged):

- `ViewFile::bytes`: `&'a std::sync::Arc<[u8]>` -> `&'a blaze_vfs::Bytes`.
- `plan_view`: `&view.files[it.at].bytes[..]` is unchanged (`Bytes: Deref<Target = [u8]>`).
  Before the plan is returned, a file with text that has no `Arc` stops the view:
  ```rust
  if let Some(file) = view.files.iter().find(|it| it.bytes.shared().is_none()) {
      return Err(ViewError::Mapped(file.path.to_owned()));
  }
  ```
  with a new variant `ViewError::Mapped(String)`, whose `Display` is
  `"{path} is a range of the mapped image, which a file's text can't hold without a copy yet"`.
  This is the no-fallback error until `FileText` holds a `Text` (the coordinator request
  "frontend-text: one `Text` from the VFS to the parse tree" in `v2/requests/frontend.md`). It
  can't fire before image restore is wired, because every file is a heap file until then.
- `write_view`: `blaze_vfs::text_of(std::sync::Arc::clone(view.files[it.at].bytes)).ok()` ->
  `blaze_vfs::text_of(view.files[it.at].bytes.clone()).ok().map(|it| it.shared().expect("checked in `plan_view`"))`,
  still a `Vec<Option<std::sync::Arc<str>>>`. Everything else in section 3 stays as written.
- `tests.rs`: `vfs_file` makes `blaze_vfs::Bytes::from(arc)`, keeping the `Arc<[u8]>` for the
  pointer assert.

Load-cargo's side (it matches `ViewError` exhaustively) is the amendment of the same name in
`v2/requests/cargo.md`.
Status: open

> forwarded to db (window `db-next2`, told by tmux).
> frontend-hooks (2026-10-29): superseded by "the parse tree's source is a `Text`" §2 below (no
> `ViewError::Mapped`, no `shared()`), which is applied; its `ViewFile::bytes: &blaze_vfs::Bytes`
> is in.

## frontend-nameres-graph: proptest for hir-def's tests (2026-10-29)

What: `crates/hir-def/Cargo.toml`, under `[dev-dependencies]`, after `expect-test.workspace = true`:
`proptest.workspace = true`.
Why: `nameres/facts/tests/graph.rs` (frontend-nameres-graph) and `nameres/facts/tests/generated.rs`
(frontend-nameres-diff) are proptest property tests. The workspace dependency is asked of the
coordinator in `v2/requests/frontend.md` (`## frontend-nameres-graph: proptest as a workspace
dependency`). The allocation-budget test in `facts/tests/graph.rs` also needs speed-hir-store's
`## speed-hir-store: one counting allocator for hir-def's tests` items 2-4 above (it calls
`blaze_alloc_count::measure`).
Status: open

> forwarded to db (window `db-next2`, told by tmux); the root `proptest` line is in (requests).
> applied by frontend-hooks (Andrew's decision 2026-10-29 20:45): `proptest.workspace = true`
> after `expect-test` in `crates/hir-def/Cargo.toml` `[dev-dependencies]` (manifest lint clean).
> The counting allocator (speed-hir-store items 2-4) is not part of this section.

## frontend-nameres-impls: re-export module_impls (2026-10-29)

What: `crates/hir-def/src/nameres.rs`, right after the line
`pub use self::path_resolution::ResolvePathResultPrefixInfo;` (~L98), add:

```rust
pub use self::facts::impls::{ModuleImpls, module_impls};
```

Lands together with frontend-nameres-core's `mod facts;` line in the same file (core's request
under `## frontend-nameres-core:` in this file): without `mod facts;` the path does not exist,
and `facts.rs` declares `pub(crate) mod impls;`, so with it the line compiles. `ModuleImpls` and
`module_impls` are `pub` items in `crates/hir-def/src/nameres/facts/impls.rs` (written, ready).
Why: frontend.md items 2 and 3. `module_impls(db, module: ModuleId) -> &ModuleImpls` is one
module's impls (trait, inherent, builtin derive) in declaration order, a tracked query that is
equal (backdated) unless an impl is added to or removed from that module. hir-ty's impl tables
and blaze-items' indexes build their buckets from it per module (requests in
`v2/requests/items.md`, `## frontend-nameres-impls: per-module impl collection`), so they reach
it as `hir_def::nameres::{ModuleImpls, module_impls}`.
Tests: `crates/hir-def/src/nameres/facts/tests/impls.rs` (declared by frontend-nameres-diff's
`facts/tests.rs`). Its allocation-budget test calls `blaze_alloc_count::assert_at_most`, so it
also needs speed-hir-store's `## speed-hir-store: one counting allocator for hir-def's tests`
items 2-4 above.
Status: open

> forwarded to db (window `db-next2`, told by tmux).
> left for db-expand-names-next2 (frontend-hooks, 2026-10-29): owns `nameres.rs` (running, file
> touched minutes before); lands with frontend-nameres-core's `mod facts;`. Told by tmux.
> applied by db-expand-names (2026-10-29 ~21:00): the line after
> `ResolvePathResultPrefixInfo`, with frontend-nameres-core's `mod facts;` (also applied). Status: applied.

## speed-hir-store-stmts: statements become columns, `Statement` a view (hir-ty, hir-ty-borrowck, hir-ty-unsafeck)

From speed-hir-store-stmts (child of speed-hir-store), 2026-10-29. `hir-def` (`hir.rs`,
`hir/columns.rs`) stores a block's statements as rows of two columns (`stmt_kind: [u8]`,
`stmt_data: [u32]`, 5 bytes a statement; a `let`'s ids in `extra`, macro definitions in a side
table), where it was a 32-byte enum in a flat array. These land together with that change.

**The rule.** `Statement` is now `Statement<'a>`, a `Copy` view decoded from the rows, with
today's variant and field names (`Let { pat, type_ref, initializer, else_branch, is_super }`,
`Expr { expr, has_semi }`, `Item(Item<'a>)` with `Item::MacroDef(&'a MacroDefId)` / `Item::Other`).
`Expr::Block { statements, .. }` gives `statements: Statements<'a>`, a `Copy` handle (`len`,
`is_empty`, `get(i) -> Option<Statement<'a>>`, `first`, `last`, `iter()` double-ended and exact
size, `IntoIterator` for it and for `&` it), no longer `&'a [Statement]`. So:
1. A statement is a value. `for stmt in statements` and `statements.iter()` still compile and
   yield `Statement<'_>`; `get`/`first`/`last` give `Option<Statement<'_>>`. Its id, `Option`
   and `bool` fields bind by value: `*pat` -> `pat`, `*initializer` -> `initializer`,
   `Some(*e)` -> `Some(e)`; `match *stmt` -> `match stmt`; a `&Statement::..` pattern drops
   the `&`. `Item::MacroDef(id)` still binds `&MacroDefId`.
2. A parameter `&[Statement]` -> `Statements<'_>` (`hir_def::hir::Statements`), `&Statement`
   -> `Statement<'_>`.
3. No slice operations: a slice pattern on `statements` becomes `is_empty`/`get`; no site in
   these crates uses one. `.iter().any/all/filter_map/enumerate` work as before (the closure
   gets a `Statement<'_>`; `matches!(it, Statement::Item(_))` is unchanged).
4. Nothing outside hir-def builds a `Statement`.

Examples (real sites):
- `crates/hir-ty/src/infer/expr.rs` ~L1499: `statements: &[Statement],` -> `statements: Statements<'_>,`
- `crates/hir-ty/src/infer/closure/analysis.rs` ~L324: `let Statement::Let { pat, initializer: Some(init), .. } = *stmt else {` -> `let Statement::Let { pat, initializer: Some(init), .. } = stmt else {`
- `crates/hir-ty-borrowck/src/regions.rs` ~L2181: `found.push((*pat, *initializer));` -> `found.push((pat, initializer));`

Counts (sites): hir-ty 14 in 4 files (`infer/expr.rs` 7, `infer/mutability.rs` 4,
`infer/closure/analysis.rs` 1, `infer/closure/analysis/expr_use_visitor.rs` 2; `consteval.rs`
none), hir-ty-borrowck 17 in 3 files (`scan.rs` 7, `check.rs` 4, `regions.rs` 6; `loans.rs`,
`qualifs.rs` none), hir-ty-unsafeck 1 (`lib.rs`). `crates/hir/` and
`crates/hir-ty-wfcheck/` have none. `check.rs` was being edited when this was written: its line
numbers may have moved, the old texts are exact.

> forwarded to db (window `db-next2`, told by tmux).

### crates/hir-ty/src/infer/expr.rs  (7)
- ~L12  `RecordSpread, Statement, UnaryOp,`  ->  `RecordSpread, Statement, Statements, UnaryOp,`
- ~L1499 (in `fn infer_block`)  `        statements: &[Statement],`  ->  `        statements: Statements<'_>,`
- ~L1516  `.unwrap_or_else(|| this.table.next_ty_var((*pat).into()));`  ->  `.unwrap_or_else(|| this.table.next_ty_var(pat.into()));`
- ~L1520-1521 (the `this.infer_let(` call)  `                                *initializer,\n                                *pat,`  ->  `                                initializer,\n                                pat,`
- ~L1527 (in `if let Some(expr) = else_branch {`)  `                                    *expr,\n                                    &Expectation::HasType(this.types.types.never),`  ->  `                                    expr,\n                                    &Expectation::HasType(this.types.types.never),`
- ~L1534  `&Statement::Expr { expr, has_semi } => {`  ->  `Statement::Expr { expr, has_semi } => {`
- unchanged: `type_ref.as_ref().map(|&tr| this.make_ty(tr))` (`Option<TypeRefId>` by value still has `as_ref`), the call `self.infer_block(tgt_expr, statements, tail, label, expected)` at ~L404.

> forwarded to db (window `db-next2`, told by tmux).

### crates/hir-ty/src/infer/mutability.rs  (4, all in the `Expr::Block` arm, ~L100-112)
- `self.infer_mut_expr(*i, self.pat_bound_mutability(*pat));`  ->  `self.infer_mut_expr(i, self.pat_bound_mutability(pat));`
- `self.infer_mut_expr(*e, Mutability::Not);`  ->  `self.infer_mut_expr(e, Mutability::Not);`
- `                            self.infer_mut_expr(*expr, Mutability::Not);` (the `Statement::Expr` arm, ~L111)  ->  `                            self.infer_mut_expr(expr, Mutability::Not);`

> forwarded to db (window `db-next2`, told by tmux).

### crates/hir-ty/src/infer/closure/analysis.rs  (1)
- ~L324  `let Statement::Let { pat, initializer: Some(init), .. } = *stmt else {`  ->  `let Statement::Let { pat, initializer: Some(init), .. } = stmt else {`

> forwarded to db (window `db-next2`, told by tmux).

### crates/hir-ty/src/infer/closure/analysis/expr_use_visitor.rs  (2)
- ~L694  `    fn walk_stmt(&mut self, stmt: &Statement) -> Result {`  ->  `    fn walk_stmt(&mut self, stmt: Statement<'_>) -> Result {`
- ~L695  `        match *stmt {`  ->  `        match stmt {`
- unchanged: `for stmt in statements { self.walk_stmt(stmt)?; }` at ~L638.

> forwarded to db (window `db-next2`, told by tmux).

### crates/hir-ty-borrowck/src/scan.rs  (7, the `Expr::Block` arm, ~L143-162)
- `matches!(store.pat(*pat), Pat::Bind { subpat: None, .. });`  ->  `matches!(store.pat(pat), Pat::Bind { subpat: None, .. });`
- `let scrutinee = if simple { None } else { Some(*init) };`  ->  `let scrutinee = if simple { None } else { Some(init) };`
- `store.walk_bindings_in_pat(*pat, |binding| {`  ->  `store.walk_bindings_in_pat(pat, |binding| {`
- `Statement::Expr { expr, .. } => match store.expr(*expr) {`  ->  `Statement::Expr { expr, .. } => match store.expr(expr) {`
- `let value = erased(infer.expr_ty(*expr));`  ->  `let value = erased(infer.expr_ty(expr));`
- `statement_values.insert(*expr, Some(value));`  ->  `statement_values.insert(expr, Some(value));`
- `statement_values.insert(*expr, None);`  ->  `statement_values.insert(expr, None);`

> forwarded to db (window `db-next2`, told by tmux).

### crates/hir-ty-borrowck/src/check.rs  (4)
- in `fn let_bound_block_value` (~L355-357): `if matches!(ctx.store.pat(*pat), Pat::Bind { subpat: None, .. })`  ->  `if matches!(ctx.store.pat(pat), Pat::Bind { subpat: None, .. })`; `&& matches!(ctx.store.expr(*init), Expr::Block { .. })`  ->  `&& matches!(ctx.store.expr(init), Expr::Block { .. })`; `&& value(*init) == expr`  ->  `&& value(init) == expr`
- ~L3065  `                        Statement::Let { pat, .. } => Some(*pat),`  ->  `                        Statement::Let { pat, .. } => Some(pat),`

> forwarded to db (window `db-next2`, told by tmux).

### crates/hir-ty-borrowck/src/regions.rs  (6)
- ~L2182  `&& spelled(self.ctx.store.pat(*pat))`  ->  `&& spelled(self.ctx.store.pat(pat))`
- ~L2184  `found.push((*pat, *initializer));`  ->  `found.push((pat, initializer));`
- ~L2443-2446 (in `fn lets`)  `pat: *pat,\n                        type_ref: *type_ref,\n                        initializer: *initializer,`  ->  `pat,\n                        type_ref,\n                        initializer,`

> forwarded to db (window `db-next2`, told by tmux).

### crates/hir-ty-unsafeck/src/lib.rs  (1)
- ~L519  `                    match *statement {`  ->  `                    match statement {`

Status: open

> forwarded to db (window `db-next2`, told by tmux).

## frontend-deps-sigs: serve signatures from a dependency image (frontend.md item 5; 2026-10-29)

`crates/hir-def/src/deps/signatures.rs` (lane `frontend-deps-sigs`, declared by `deps.rs` as
`pub(crate) mod signatures`) writes the signatures part of a dependency image and serves each
signature query from it. It needs four changes in db's paths. Nothing else in db's paths moves;
without an image every `served_*` is `None` and the queries behave as today.

> forwarded to db (hir-def is its; told by tmux).

### 1. `crates/hir-def/src/nameres/frozen.rs`: five id codes visible to the crate

`deps/signatures/table.rs` encodes a record's relocation table with frozen's `Enc`/`Dec`, so a
table names files, macro calls, ast ids and hygiene contexts exactly as the def map part does.
These are private today:

- `Enc` (L671, L693, L706, L716, L874): `fn file(`, `fn hir_file(`, `fn ast_id<`, `fn call(`,
  `fn context(` -> `pub(crate) fn file(` and so on, bodies unchanged.
- `Dec` (L2086, L2093, L2101, L2111, L2218): the same five -> `pub(crate) fn`.
- Why: one encoding of a portable id, not a second copy of it in `deps/`.

> forwarded to db (hir-def is its; told by tmux).
> left for db-expand-names-next2 (frontend-hooks, 2026-10-29): owns `nameres/frozen.rs` (running,
> touched 2 min before); the ten methods are still private there. Told by tmux.
> applied by db-expand-names (2026-10-29 ~21:06): the ten methods (`Enc` and `Dec`'s `file`,
> `hir_file`, `ast_id`, `call`, `context`) are `pub(crate)`, bodies unchanged. Every other
> `Enc`/`Dec` method `deps/signatures/table.rs` calls was already `pub`. Status: applied.

### 2. `crates/hir-def/src/signatures.rs`: the hook at the head of each `of`

In each of `StructSignature::of` (L439), `UnionSignature::of` (L541), `EnumSignature::of`
(L649), `ConstSignature::of` (L754), `StaticSignature::of` (L849), `ImplSignature::of` (L943),
`TraitSignature::of` (L1061), `FunctionSignature::of` (L1181), `TypeAliasSignature::of`
(L1372) and `VariantFields::of` (L1580):

- Old:
  ```rust
  pub fn of(db: &dyn SourceDatabase, id: StructId) -> Arc<Self> {
      match Self::relative(db, id) {
  ```
- New:
  ```rust
  pub fn of(db: &dyn SourceDatabase, id: StructId) -> Arc<Self> {
      if let Some(it) = crate::deps::signatures::served_struct(db, id) {
          return it;
      }
      match Self::relative(db, id) {
  ```
  with `served_union`, `served_enum`, `served_const`, `served_static`, `served_impl`,
  `served_trait`, `served_function`, `served_type_alias`, `served_variant_fields` in the others.
- Why: the query, not each caller, decides where the value comes from; inside `of` so a block
  the record names is made by the query (blocks are tracked structs).

> forwarded to db (hir-def is its; told by tmux).
> applied by frontend-hooks (Andrew's decision 2026-10-29 20:45): the hook at the head of all ten
> `of`s in `crates/hir-def/src/signatures.rs` (`of(db: &Db, ..)` now, before `match Self::relative`).

### 3. `crates/hir-def/src/signatures.rs`: `EnumVariants::of` split into hook and lowering

- Old (L1772-1820): `#[salsa::tracked(returns(ref))] pub(crate) fn of(db, e) -> (EnumVariants,
  ThinVec<InactiveEnumVariantCode>) { let loc = e.lookup(db); ... (EnumVariants { variants,
  explicit_discriminant }, diagnostics) }`
- New:
  ```rust
  #[salsa::tracked(returns(ref))]
  pub(crate) fn of(
      db: &dyn SourceDatabase, // dyn: salsa's query database, `dyn` across the front end
      e: EnumId,
  ) -> (EnumVariants, ThinVec<InactiveEnumVariantCode>) {
      if let Some(it) = crate::deps::signatures::served_enum_variants(db, e) {
          return it;
      }
      Self::lower(db, e)
  }
  ```
  and, in a plain (untracked) `impl EnumVariants`, `pub(crate) fn lower(db: &Db, e: EnumId)
  -> (EnumVariants, ThinVec<InactiveEnumVariantCode>)` holding the old body unchanged. (`of`'s own
  `db` parameter becomes `&Db` with the rest of the file in the `&Db` migration; `served_*` and
  `lower` take `&Db` already, as new code must.)
- Why: checking an image (`Mode::Check`) lowers the variants from syntax to compare with the
  record, which `of` itself no longer does once it serves.

> forwarded to db (hir-def is its; told by tmux).
> applied by frontend-hooks (Andrew's decision 2026-10-29 20:45): `EnumVariants::of` is the hook
> then `Self::lower(db, e)`; `lower(db: &Db, e)` in a new plain `impl EnumVariants` holds the old
> body unchanged.

### 4. `crates/hir-def/src/signatures.rs`: build the two values from their parts

`VariantFields::fields` and `EnumVariants::explicit_discriminant` are private, so a decoded
record cannot be made into the value. In `impl VariantFields` and `impl EnumVariants` (plain
impls):

```rust
/// Fields read back from a dependency image.
pub(crate) fn from_parts(
    fields: Arena<FieldData>,
    store: ExpressionStore,
    shape: FieldsShape,
) -> VariantFields {
    VariantFields { fields, store, shape }
}
```

```rust
/// Variants read back from a dependency image; `explicit[i]` is whether variant `i` is
/// written with `= expr`.
pub(crate) fn from_parts(
    variants: FxIndexMap<Name, (EnumVariantId, FieldsShape)>,
    explicit: &[bool],
) -> EnumVariants {
    let mut explicit_discriminant = VariantBits::with_len(variants.len());
    for (index, &it) in explicit.iter().enumerate() {
        explicit_discriminant.set(index, it);
    }
    EnumVariants { variants, explicit_discriminant }
}
```

- Why: the decoder (`deps/signatures/codes.rs`) builds each value it reads; the bits stay
  private to `signatures.rs`. An enum with no variants gets `with_len(0)`, an empty `SmallVec`,
  equal to the `VariantBits::default()` lowering gives it.

Already asked by `speed-hir-store` above (one counting allocator for hir-def's tests): the
allocation tests in `deps/signatures/tests.rs` use `blaze_alloc_count::assert_at_most`.

Status: open

> forwarded to db (hir-def is its; told by tmux).
> applied by frontend-hooks (Andrew's decision 2026-10-29 20:45): §4, both `from_parts` as written
> (`impl VariantFields` before `len`, `impl EnumVariants` after `lower`) in
> `crates/hir-def/src/signatures.rs`; §2-3 above. §1 left for db-expand-names-next2.

## frontend-nameres-core: crate_local_def_map and block_def_map on the per-module resolver

From `frontend-nameres-core` (report `/tmp/blaze-lanes/v2-frontend-nameres-core.md`, notes
`docs/v2/plan/lanes/frontend/nameres-core.md`). Puts the def maps on `nameres/facts/` (the
per-module, per-SCC resolver of `docs/v2/plan/lanes/frontend/nameres.md`) and takes the old
`collector` fixpoint off the build's path (test-only, for the differential tests). Every edit
below is needed together; `facts/` is written against them.

> forwarded to db (hir-def is its; told by tmux).

### 1. `crates/hir-def/src/nameres.rs`, module lines

old:
```rust
pub mod attr_resolution;
mod collector;
pub mod demand;
```
new:
```rust
pub mod attr_resolution;
#[cfg(test)]
mod collector;
pub mod demand;
mod facts;
pub use self::facts::kinds;
```
Why: `collector` stays only as the differential tests' reference; `facts` is the resolver;
`kinds` gives `blaze-v2-proof` the resolver's `QueryKind`s (lane oracle's ask). (The impls
child's `pub use self::facts::impls::{ModuleImpls, module_impls};` is its own request.)

> forwarded to db (hir-def is its; told by tmux).

### 2. `crates/hir-def/src/nameres.rs`, `crate_local_def_map` body

old (from `if let Some((def_map, local_def_map)) = frozen::served(db, crate_id) {` to the end of
the fn):
```rust
    if let Some((def_map, local_def_map)) = frozen::served(db, crate_id) {
        return DefMapPair::new(db, def_map, local_def_map);
    }
    let root_file_id = crate_id.root_file_id(db);
    let module_data = ModuleData::new(
        ModuleOrigin::CrateRoot { definition: root_file_id },
        Visibility::Public,
        None,
    );

    let def_map = DefMap::empty(
        db,
        crate_id,
        Arc::new(DefMapCrateData::new(krate.edition)),
        module_data,
        None,
    );
    let (mut def_map, local_def_map) =
        collector::collect_defs(db, def_map, TreeId::new(root_file_id.into(), None), None);
    def_map.hand_over_scopes(db);
    frozen::check(db, crate_id, &def_map, &local_def_map);

    DefMapPair::new(db, def_map, local_def_map)
}
```
new:
```rust
    if let Some((def_map, local_def_map)) = frozen::served(db, crate_id) {
        return DefMapPair::new(db, def_map, local_def_map);
    }
    let (def_map, local_def_map) = facts::collect_crate(db, crate_id);
    frozen::check(db, crate_id, &def_map, &local_def_map);

    DefMapPair::new(db, def_map, local_def_map)
}
```
and delete the now-unused first line of the body, `let krate = crate_id.data(db);` (the
tracing span reads `crate_id` only). `collect_crate` returns the map with scopes handed over
and `diagnostics_by_module` made.

> forwarded to db (hir-def is its; told by tmux).

### 3. `crates/hir-def/src/nameres.rs`, `block_def_map` body end

old:
```rust
    let (mut def_map, _) = collector::collect_defs(
        db,
        def_map,
        TreeId::new(ast_id.file_id, Some(block_id)),
        Some(local_def_map.local(db)),
    );
    def_map.hand_over_scopes(db);
    def_map
}
```
new:
```rust
    facts::collect_block(
        db,
        def_map,
        TreeId::new(ast_id.file_id, Some(block_id)),
        local_def_map.local(db),
    )
}
```

> forwarded to db (hir-def is its; told by tmux).

### 4. `crates/hir-def/src/nameres.rs`, `ModulesSlot`: a resolver view

old:
```rust
enum ModulesSlot {
    /// While the `DefMap` is being collected.
    Building(ModulesMap),
    /// A `Modules`, once it is.
    Collected(salsa::Id),
}
```
new:
```rust
enum ModulesSlot {
    /// While the `DefMap` is being collected.
    Building(ModulesMap),
    /// A `Modules`, once it is.
    Collected(salsa::Id),
    /// A view of the per-module resolver (`facts`): the modules it holds, and every other
    /// module of its crate read through `facts::row` of `rows` (a `facts` `RowsKey`).
    Unit { own: ModulesMap, rows: salsa::Id },
}
```
In `impl ModulesSlot`, `fn building`: old `ModulesSlot::Building(modules) => modules,` new
`ModulesSlot::Building(modules) | ModulesSlot::Unit { own: modules, .. } => modules,`.

In `DefMap::module`, old:
```rust
        let found = match &self.modules {
            ModulesSlot::Building(modules) => modules.get(&id),
```
new:
```rust
        let found = match &self.modules {
            ModulesSlot::Building(modules) => modules.get(&id),
            ModulesSlot::Unit { own, rows } => match own.get(&id) {
                Some(it) => Some(it),
                None => facts::row(db, *rows, id),
            },
```
In `DefMap::table`, after the `ModulesSlot::Building(modules) => modules,` arm add:
```rust
            ModulesSlot::Unit { .. } => panic!("the modules of a resolver view are not listed"),
```
Why: a unit (one strongly connected component of a round) holds its members and reads every
other module's row lazily, so a unit costs its members, not the crate's module count. Path
resolution and visibility only call `DefMap::module` (checked: no `table`/`modules` call in
`path_resolution.rs` or `visibility.rs`). `shrink_to_fit` and `hand_over_scopes` match
`Building` only and stay as they are (views are never handed over).

> forwarded to db (hir-def is its; told by tmux).

### 5. `crates/hir-def/src/nameres.rs`, `ScopeSlot`: a scope read as a fact

old:
```rust
enum ScopeSlot {
    /// While the `DefMap` is being collected.
    Building(Box<BuildingScope>),
    /// A `ModuleScope`, once it is.
    Collected(salsa::Id),
}
```
new:
```rust
enum ScopeSlot {
    /// While the `DefMap` is being collected.
    Building(Box<BuildingScope>),
    /// A `ModuleScope`, once it is.
    Collected(salsa::Id),
    /// The `ModuleScope` a resolver fact names (a `facts` `ScopeRef`), looked up on first read.
    Fact(salsa::Id),
}

impl ScopeSlot {
    /// The `ModuleScope` of a handed-over or fact slot.
    fn handle(&self, db: &dyn SourceDatabase) -> Option<salsa::Id> {
        match self {
            ScopeSlot::Building(_) => None,
            ScopeSlot::Collected(id) => Some(*id),
            ScopeSlot::Fact(fact) => Some(facts::fact_scope(db, *fact)),
        }
    }
}
```
Then in `impl ModuleData`, in each of `scope`, `name_in_scope`, `legacy_macro_in_scope`,
`traits_in_scope`, `children`: replace the `ScopeSlot::Collected(id) =>` arm's pattern by
`ScopeSlot::Collected(_) | ScopeSlot::Fact(_) =>` and, in that arm's body, `*id` by
`self.scope.handle(db).expect("a handed-over slot")`. For example `scope`:
```rust
            ScopeSlot::Collected(_) | ScopeSlot::Fact(_) => {
                let id = self.scope.handle(db).expect("a handed-over slot");
                <ModuleScope<'a> as salsa::plumbing::FromId>::from_id(id).scope(db)
            }
```
In `children_mut` and `scope_mut`, old `ScopeSlot::Collected(_) =>` new
`ScopeSlot::Collected(_) | ScopeSlot::Fact(_) =>` (same panic). The size assert
(`size_of::<ScopeSlot>() <= 16`) still holds. Why: a unit reads another component's module
through its round's `fact_handle` only when a path reaches it, so the salsa edge is per name
read (`ModuleScope::name`), the fact of `nameres.md` §3. `frozen.rs` matches on `ScopeSlot`?
(`rg -n 'ScopeSlot::' crates/hir-def/src` shows only `nameres.rs`; if `frozen.rs` matches it
exhaustively, give `Fact` the `Collected` arm's body through `handle`.)

> forwarded to db (hir-def is its; told by tmux).

### 6. `crates/hir-def/src/nameres/mod_resolution.rs`: `ModDir` is a key

old: `#[derive(Clone, Copy, Debug)]` above `pub(super) struct ModDir {` and above
`struct DirPath(Symbol);`
new: `#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]` on both.
Why: a module's round-0 key (`facts::queries::Decl`) and its `SetModDir` op hold its `ModDir`
(16 bytes, an interned symbol: comparing is two words).

> forwarded to db (hir-def is its; told by tmux).

### 7. `crates/hir-def/src/item_scope.rs`: `PerNsGlobImports` is carried

old: `#[derive(Debug, Default)]` above `pub struct PerNsGlobImports {`
new: `#[derive(Debug, Default, Clone, PartialEq, Eq)]`
Why: a unit carries its glob-import marks from one round to the next (`// clone:` there:
once per round a component changes), and a macro step's view starts from its module's.

> forwarded to db (hir-def is its; told by tmux).

### 8. `crates/hir-def/Cargo.toml`: blaze-query

In `[dependencies]` add `blaze-query.workspace = true` (it is a workspace dependency already;
blaze-query does not depend on hir-def). Why: `facts/kinds.rs` gives the resolver's queries
`blaze_query::QueryKind` numbers, as `hir_ty::db::kinds` does.

> forwarded to db (hir-def is its; told by tmux).
> applied by frontend-hooks (Andrew's decision 2026-10-29 20:45): `blaze-query.workspace = true`
> after `blaze-phf` in `crates/hir-def/Cargo.toml` `[dependencies]` (manifest lint clean).

### 9. Other `collector::` uses

`rg -n 'collector::' crates/hir-def/src` finds, besides the two bodies above, only
`nameres/tests/worklist.rs` (a test module, `#[cfg(test)]` through `mod tests`), which keeps
working with `#[cfg(test)] mod collector;`. Nothing else to move.

Status: open

> forwarded to db (hir-def is its; told by tmux).
> left for db-expand-names-next2 (frontend-hooks, 2026-10-29 ~20:55): sections 1-7 edit
> `nameres.rs` (touched 2 min before), `nameres/mod_resolution.rs` and `item_scope.rs` (10 min),
> all owned by the running db-expand-names-next2; told by tmux. Section 8 is applied (above).
> applied by db-expand-names (2026-10-29 ~21:00): sections 1-7. 1-4 and 6 as written. 5 with
> `ScopeSlot::handle(&self, db: &Db)` (not `&dyn SourceDatabase`: no `dyn`, every function takes
> `&Db`); `frozen.rs` does not match on `ScopeSlot`. 7: `PerNsGlobImports` is now
> `PerNsGlobImports<A: Allocator + Clone = Global>` on `hashbrown` (mem-db-def-resolve R3), so it
> derives `Clone` and has hand-written `PartialEq`/`Eq` (a derive would need `Global: PartialEq`,
> which it isn't); its `Debug`/`Default` were already hand-written. Not built (lane rule).
> Status: applied.

## frontend-text: the parse tree's source is a `Text`; `FileText` and `BodySyntax` hold one (2026-10-29)

Coordinator request "one `Text` from the VFS to the parse tree" (`v2/requests/frontend.md`).
`crates/blaze-bytes` holds `Bytes` and `Text` (checked UTF-8, `Deref<Target = str>`, `Clone` by
one count, `Eq`/`Hash` as the `str`, `From<Arc<str>>` with no copy, `ptr_eq`). The syntax side is
written: `FlatTree::source() -> &syntax::Text`, and `SourceFile::parse_shared`, `parse_outline`,
`parse_file` and `Parse::reparse_to` take `impl Into<syntax::Text>`, so a caller that passes an
`Arc<str>` compiles unchanged.

> forwarded to db (told by tmux).

### 1. Breaks now (two lines)

- `crates/base-db/src/tests.rs:266`:
  `assert!(std::sync::Arc::ptr_eq(tree.source(), db.file_text(files[1]).text(&db)));` becomes
  `assert!(tree.source().as_ptr() == db.file_text(files[1]).text(&db).as_ptr());`, and
  `assert!(tree.source().ptr_eq(db.file_text(files[1]).text(&db)));` once section 2 lands.
- `crates/hir-def/src/expr_store/body.rs:427`:
  `ast::SourceFile::parse_outline(std::sync::Arc::from(self.text()), self.edition)` becomes
  `ast::SourceFile::parse_outline(self.text(), self.edition)`. `Arc::from` no longer infers
  against `impl Into<Text>`. A `&str` is a copy into an `Arc`, as before.

`crates/hir-def/src/signatures.rs:174-180` (`replace_subtree(at, &empty.finish(), text.into())`
from a `String`) compiles unchanged.

> forwarded to db (told by tmux).

### 2. `FileText` holds a `Text`, and the `ViewError::Mapped` gate goes

This replaces the amendment above, "section 3: `Bytes` and `Text`", in two places:
`ViewError::Mapped` and the `.shared().expect(...)` in `write_view` are no longer wanted. An
image file's text is held as it is, with no copy and nothing refused.

- `crates/base-db/src/lib.rs`: `FileText`'s field `text: std::sync::Arc<str>` becomes
  `text: syntax::Text`, and `FileText::text` returns `&'db syntax::Text`. Where it is set
  (`FileText::new` / `set_text_field`), an `Arc<str>` becomes `syntax::Text::from(arc)`.
- `view.rs`, `write_view`: `blaze_vfs::text_of(view.files[it.at].bytes.clone()).ok()` is the
  `Option<Text>` it stores (a `Vec<Option<syntax::Text>>`). There is no `shared()`, and
  `plan_view` gets no `ViewError::Mapped`. `ViewFile::bytes: &blaze_vfs::Bytes` stays as the
  amendment says. Until frontend-journal-tree's "`Bytes` moves to blaze-bytes" (in
  `v2/requests/frontend-journal.md`) lands, `blaze_vfs::Bytes` is not yet `blaze_bytes::Bytes`.
- `crates/hir-def/src/expr_store/body.rs`: `BodySyntax::text: Arc<str>` becomes
  `syntax::Text`. Its fill at ~L666 (`db.file_text(file).text(db).clone()`) is unchanged.
- `EditionedFileId::parse` passes the text to `SourceFile::parse_file`, which takes it as it is.
- Outside db, these keep compiling but get a `Text` where they had an `Arc<str>`:
  `blaze-doc/src/source.rs:79` and `blaze-engine/src/lib.rs:4039` (`.text(db).clone()`).
  `blaze-doc/src/source.rs:51` (`Arc::from(&**...)`) compiles as is.
Status: open

> forwarded to db (told by tmux).
> applied by frontend-hooks (Andrew's decision 2026-10-29 20:45), base-db's part of §1-2 and of
> "one lazy line index" §3, re-derived against the tree as it is now (db's D8 reparse,
> `write_text`'s `(text, file, content)`):
> - `lib.rs`: `FileText`'s field is `text: syntax::Text`; `FileText::text` returns
>   `&'db syntax::Text`.
> - `files.rs`: `write_text` takes `Option<(syntax::Text, ContentId, ContentId)>` and stores it as
>   given (the `Arc::from(text)` copy is gone); `set_text` (a `&str` from outside a view) makes
>   `syntax::Text::from(text)`, one copy as before; a file losing its text gets
>   `Text::from_static("")` (no allocation).
> - `reparse.rs`: `Slot::text`, `Reparse::{new, after, parse}`, `whole` and `assert_whole` take
>   `Text`/`&Text`; the debug check is `text.ptr_eq(&self.slot.text)`.
> - `view.rs`: `ViewFile { path, id: ContentId, bytes: &'a blaze_vfs::Bytes }`; `plan_view` reads
>   `file.id` and `&bytes[..]`; `write_view` keeps `blaze_vfs::text_of(bytes.clone()).ok()`, a
>   `Vec<Option<syntax::Text>>`, and matches `(&text, ..)`. No `ViewError::Mapped`.
> - `tests.rs`: `vfs_bytes` + `view_file` replace `vfs_file` at the four view sites;
>   `a_files_parse_shares_its_text` asserts `tree.source().ptr_eq(..)`; new
>   `a_views_text_is_its_bytes` (the text's pointer is the view's bytes').
> - `budget_tests.rs`: `texts` makes `blaze_vfs::Bytes`, `view_files` the new `ViewFile`, and
>   `PER_CHANGED_FILE` is 1 (the reparse slot; the text is no longer a copy).
> - `editioned_file_id.rs`: nothing; `reparse_field(db).parse(input.text(db), ..)` takes `&Text`.
>   The bullet "`EditionedFileId::parse` passes the text to `SourceFile::parse_file`" is superseded
>   by D8.
> Not db's, sent on: `BodySyntax::text` and `body.rs:418` (lane format, `v2/requests/format.md`,
> "frontend-hooks: `BodySyntax::text` is a `syntax::Text`"); `hir_expand::FileRange::file_text`'s
> return type (`v2/requests/db-expand.md`, db-expand-macros-next2 told); load-cargo's
> `ViewFile { path, id, bytes }` at `v2.rs:754` (cargo, `cargo-inbox` told). Until those land
> those three files don't compile against base-db.

## frontend-tree: views of a file's tree, borrowed from the database (2026-10-29)

frontend.md item 16 (notes: `docs/v2/plan/lanes/frontend/tree.md`) moves the build path from the
counted handles (`SyntaxNode`, `ast::*`) to borrowed `Copy` views (`syntax::view`). The parse a
query keeps is returned by reference (`EditionedFileId::parse`, `parse_macro_expansion` are
`returns(ref)`), so a view of it lives as long as the `&'db Db`: no handle, no
refcount. These two additions let every caller in `hir-expand`, `hir-def`, `hir`, `hir-ty` move
one call at a time; nothing existing changes.

1. File: `crates/hir-expand/src/lib.rs`, in `impl HirFileId`, after `pub fn parse_or_expand`
   (L1823-1830). New:

   ```rust
   /// The file's root as a view, borrowed from the parse the database keeps: what
   /// [`Self::parse_or_expand`] is, with no handle made.
   pub fn root(self, db: &base_db::Db) -> syntax::view::Node<'_> {
       match self {
           HirFileId::FileId(file_id) => file_id.parse(db).root(),
           HirFileId::MacroFile(macro_file) => macro_file.parse_macro_expansion(db).value.0.root(),
       }
   }
   ```

2. File: `crates/hir-expand/src/files.rs`, after `impl<N: AstNode> AstId<N> { ... }` (ends L117).
   New:

   ```rust
   impl<N: AstNode + syntax::view::Typed> AstId<N> {
       /// The node as a view of the tree the database keeps: [`Self::to_node`] with no handle.
       pub fn to_view<'db>(&self, db: &'db base_db::Db) -> N::Of<'db> {
           self.to_ptr(db).to_view(self.file_id.root(db))
       }
   }
   ```

Why: the views are the build path's tree API from item 16 on; with these, a caller writes
`let f = id.to_view(db);` (an `ast::Fn<'db>` under `use syntax::view as ast`) where it wrote
`id.to_node(db)`. `syntax::view` is declared through frontend-deps' request.

Also, for your lanes: new code that reads syntax should read it through `syntax::view`
(`use syntax::view as ast;`, same names, methods take `self`, text is `&'t str`). When a crate
of yours (`hir-expand`, `hir-def`, `hir`, `hir-ty`, `base-db`) reports DONE, frontend-tree's
children move the rest of it; tell `frontend-tree` by tmux.

Status: open

> forwarded to db (told by tmux).
> applied by db-expand: both as written (`HirFileId::root` after `parse_or_expand` in `crates/hir-expand/src/lib.rs`, `AstId::to_view` after `impl<N: AstNode> AstId<N>` in `files.rs`); `ignored_semicolon` (lib.rs) reads the call's path through `to_view`.
> frontend-hooks (2026-10-29): nothing left; both are in (`hir-expand/src/lib.rs` `HirFileId::root`,
> `files.rs` `AstId::to_view`).

## frontend-tree: to db-expand-macros-next1, hand over `crates/span/src/ast_id.rs` (2026-10-29)

frontend-text (span's other owner) reports DONE and gives frontend-tree the move of span's public
syntax API onto views (frontend.md item 16). That API is `AstIdMap::from_source(&SyntaxNode)`,
`relative_to` and `relative_without(&SyntaxNode, &SyntaxNode, &[SyntaxNodePtr])`, plus the
item naming under them (`ErasedFileAstId::ast_id_for`, `has_name_ast_id`, the `Impl`/`Use`/
`ExternBlock`/`AsmExpr` id functions). The walks are already over `FlatTree` numbers; only the
entry points and the per-item naming still make cursors.

- File: `crates/span/src/ast_id.rs`. Your brief also lists `crates/span/`.
- What frontend-tree would do there:
  - add `from_view(root: syntax::view::Node<'_>)`, `relative_to_view` and
    `relative_without_view` beside the handle versions;
  - make the naming read `syntax::view` (no cursor per item);
  - keep the handle entry points as thin wrappers over the view ones (`node.to_flat()` gives
    the same `(tree, at)`) until hir-def and hir-expand stop calling them.
  - No change to the map, its ids or its hashing.
- Ask: answer with a `>` line. Either you don't touch `ast_id.rs` and frontend-tree may edit it,
  or you name what you are changing there and when you will be done. Until then frontend-tree
  does not edit `crates/span`.

Status: open

> db-expand-macros: I don't touch `crates/span/` (my brief gives it to format, not me) and have no edit in `ast_id.rs`; frontend-tree may edit it.

> forwarded to db-expand-macros-next1 (told by tmux).
> frontend-hooks (2026-10-29): nothing to apply; answered above (frontend-tree may edit `ast_id.rs`).

## frontend-db: the concrete database, `base_db::Db` (frontend.md item 15; 2026-10-29)

From `frontend-db` (report `/tmp/blaze-lanes/v2-frontend-db.md`, notes
`docs/v2/plan/lanes/frontend/db.md`, which say why: rustc's `TyCtxt` on salsa, with
`&'db Db` as `TyCtxt<'tcx>`). The type is written: `crates/base-db/src/db.rs` and its tests
`crates/base-db/src/db/tests.rs` (new files, frontend-db's). What follows hooks it in and moves
db's crates onto it. `tools/v2/concrete_db.py` does the mechanical part of 4 and 6 (dry run over
db's crates: 2,066 sites, 31 left for a person, each printed with what to do):

```sh
PYTHONPATH=python/typed-input/src python3 tools/v2/concrete_db.py \
  '{"root": ".", "crates": ["hir-def"], "write": true}'
```

Status: open

> forwarded to db (db-next2 told); the root `inventory` line is in (requests).

### 1. salsa: tracked functions over a concrete database (`vendor/salsa*`)

Today a tracked function's database must be `&dyn Trait` (`salsa-macros/src/tracked_fn.rs`,
`validity_check`, ~L440 "must have type `&dyn Db`"), and `setup_tracked_fn!` writes `dyn $Db`
in 14 places. Make it take `&Db` for a `#[salsa::db] struct Db` too:

- `vendor/salsa/src/views.rs`, exported from `plumbing`:
  ```rust
  /// What a tracked function's database parameter is: `dyn Trait` for a `#[salsa::db]` trait,
  /// or a `#[salsa::db]` struct itself.
  pub trait DbView: Any {
      fn zalsa_register_downcaster(&self) -> &DatabaseDownCaster<Self>;
  }
  ```
- `salsa-macros/src/db.rs`: for a `#[salsa::db]` trait, also emit
  `impl salsa::plumbing::DbView for dyn #TraitPath { #[inline(always)] fn zalsa_register_downcaster(&self) -> &salsa::plumbing::DatabaseDownCaster<Self> { <dyn #TraitPath as #TraitPath>::zalsa_register_downcaster(self) } }`;
  for a `#[salsa::db]` struct, emit
  `impl salsa::plumbing::DbView for #Name { #[cold] #[inline(never)] fn zalsa_register_downcaster(&self) -> &salsa::plumbing::DatabaseDownCaster<Self> { salsa::plumbing::views(self).add::<Self, Self>(|db| db) } }`
  (the identity view: `Concrete = DbView = Self`).
- `salsa-macros/src/tracked_fn.rs`, `extract_db_path`: besides `&dyn Path`, accept `&Path`
  (`syn::Type::Path`, not `mut`), and pass `setup_tracked_fn!` `DbView: [dyn #path]` or
  `DbView: [#path]` where it passes `Db: #db_path` (~L261). The message becomes "must have type
  `&dyn Db` or `&Db`, where `Db` is a Salsa database trait or a `#[salsa::db]` struct".
- `salsa-macro-rules/src/setup_tracked_fn.rs`: the parameter `Db: $Db:path` becomes
  `DbView: [$($DbView:tt)*]`; every `dyn $Db` becomes `$($DbView)*`; and
  `<dyn $Db as $Db>::zalsa_register_downcaster(db)` (L290, L294) becomes
  `<$($DbView)* as $zalsa::DbView>::zalsa_register_downcaster(db)`.
- A salsa test, `tests/tracked_fn_concrete_db.rs`: a `#[salsa::db] struct`, a tracked fn over
  `&Db` that executes once, hits, and reruns after a write; and one over `&dyn Database`
  beside it on the same database, so both views live together.

Why: with `DbView = Db`, `fn_ingredient_` is static, and a hit has no vtable call.

### 2. salsa: the task depth is a method of the database, not a process hook

`task::current_depth()` calls through `TASK_DEPTH: OnceLock<fn() -> usize>` on every hit
(`attach.rs` L70, L93, L124; `TaskLocals::current`, `task.rs` L141, under every
`zalsa_local()`).

- `Database` gets `fn task_depth(&self) -> u32 { 0 }` (one task per thread), which
  `base_db::Db` overrides (written: `blaze_graph::pump::depth`).
- `attach` and `attach_allow_change` read `db.task_depth()` before `as_dyn_database()`; the
  guards take the depth as an argument.
- `TaskLocals::current(&self)` becomes `current(&self, depth: u32)`; `storage.rs` L147's blanket
  `zalsa_local` passes `self.task_depth()` (static: the impl is per database type).
- `ZalsaLocal` records its `TaskId` when `TaskLocals::insert` makes it; `TaskId::current()`
  (`function/sync.rs` L167, L241, L298; `runtime.rs` L339, L439) becomes the caller's
  `zalsa_local.task()`. Where one of them has no `ZalsaLocal` at hand, it gets one passed down
  rather than reading a global.
- `TASK_DEPTH`, `set_task_depth` and `current_depth` are deleted.

Why: these are the hit path's last indirect calls. A `&self` method is static where the
database is concrete, and costs a vtable call only where a caller already holds `dyn Database`.

> frontend-db-base (20:30): section 1 is in salsa (speed-salsa: `DbType::Concrete`,
> `ConcreteView`, `tests/concrete_db.rs`). Section 2 is half in: `task_depth` exists and
> `attach`/`zalsa_local` read it, but `TASK_DEPTH`/`set_task_depth`/`current_depth` stay and
> `TaskId::current()` (function/sync.rs L167 L241 L298, runtime.rs L339 L439; each has a
> `ZalsaLocal` in scope) reads the hook, with debug_asserts that the two agree. No v2 build path
> calls `set_task_depth`, so a pumped task's claims carry depth 0. Asked speed-salsa-next2
> (window 19) and db-next2 to finish it; base-db's `install_plane_hooks` goes when it lands.

> speed-salsa (10-29, done in the working tree, compiles with the first build): section 2 is all
> in. `ZalsaLocal::new(task)` records its `TaskId` (made by `TaskLocals::insert` from the thread
> and the depth `current` was passed), read by `zalsa_local.task()`. `try_claim`,
> `try_claim_transferred` and the claim guard's `transfer_lock` use it; `peek_claim` and
> `TryClaimCycleHeadsIter` take the task as an argument (their callers in `execute.rs` and
> `maybe_changed_after.rs` pass `zalsa_local.task()`); `Runtime::block` and
> `Runtime::transfer_lock` take it as a parameter. `TASK_DEPTH`, `set_task_depth`,
> `current_depth` and both debug asserts are deleted, and `Database::task_depth` defaults to 0.
> `tests/task_sync.rs` pumps through a `task_depth` override now. `install_plane_hooks` can go:
> `salsa::set_task_depth` no longer exists, so `crates/base-db/src/lib.rs:325` won't compile
> until it does.

### 3. base-db: hook `Db` in

- `crates/base-db/src/lib.rs`: add `mod db;` beside `mod content;`, and
  `pub use crate::db::{Db, RunOrder, run_on_pool, set_run_order};`.
- Delete `pub trait SourceDatabase` (L311-417: `Db` has each method inherently, with the
  real `included_file`, `include_source`, `env_is_read` and `env_is_unset` that load-cargo's
  database lacked), `install_plane_hooks` (L421: request 2 replaces it) and the old
  `run_on_pool` (L439: `db.rs` has it over `&Db`).
- `crates/base-db/src/named.rs:193`: `resolve<Db: ?Sized + SourceDatabase>(db: &Db, ...)`
  becomes `resolve(db: &crate::Db, ...)` (db-expand-inputs2's resolve is in `Db::resolve_path`).
- Run the tool on base-db: `files.rs`, `view.rs`, `input.rs` and the rest take `&Db`/`&mut Db`.
- `crates/base-db/src/tests.rs`: its `TestDb` (L36 on) goes; the tests make
  `Db::for_tests()`. Its two `install_plane_hooks()` calls (L811-812) go with the test of that
  hook.
- `crates/base-db/Cargo.toml`: nothing (blaze-alloc-count is a dev-dependency already).

> applied by frontend-db-base: `mod db` and its re-exports in `lib.rs`; the tool on every base-db
> file (`change.rs`, `editioned_file_id.rs`, `files.rs`, `input.rs`, `lib.rs`, `view.rs`,
> `tests.rs`); `named.rs`'s `resolve` was already `lookup`, now `lookup(db: &Db, ..)` and
> `publish(&mut self, db: &mut Db)`; `tests.rs`'s `TestDb` is gone (`Db::for_tests()`); the old
> `run_on_pool` and `pub trait SourceDatabase` are deleted (every remaining user outside base-db
> is a file frontend-db-def/-ty/-tyx/-hir/-edge are converting). Also added `line_index(db: &Db,
> text: FileText) -> span::LineIndex` (frontend-text's request above, section 1), which
> `Db::line_column` calls and nobody had added. `install_plane_hooks` is deleted too, now that
> section 2 is in salsa. Its one caller left, `crates/ide-db/src/lib.rs:385`, is in ide, which
> this migration leaves alone (it also implements the deleted `SourceDatabase`).

### 4. hir-expand, hir-def, hir-ty-borrowck, hir-ty-wfcheck

Run the tool on each. Besides what it prints:

- hir-def's test database (`crates/hir-def/src/test_db.rs`) becomes
  `pub(crate) type TestDB = base_db::Db;`. Its helpers (`fetch_test_crate`,
  `module_at_position`, ...) become a `pub(crate) trait TestDBExt` that the file implements for
  `Db`, so the `db.fetch_test_crate()` call sites stay; they import it where they import
  `TestDB`. `log` becomes `Db::log` inside
  `base_db::set_run_order(RunOrder::LeftToReaders)` (and restore the order after), and
  `log_executed` becomes `Db::log_executed`. salsa's accessors are generic over the database, so
  a wrapper struct would not coerce there. That is why it is the type itself.
- hir-def: delete `ExpandProcAttrMacros` and `set_expand_proc_attr_macros`
  (`lib.rs` L110-123), and in `nameres/collector.rs` L1193 the arm guarded by
  `!crate::ExpandProcAttrMacros::get(self.db).enabled(self.db)`. Every v2 database sets it to
  `true`. It is rust-analyzer's editor switch, and rustc always expands attribute macros.
  Callers to drop: load-cargo (cargo's, told), blaze-items' old test database (done), and each
  `TestDB::default`.
- hir-ty-wfcheck's `Cargo.toml` needs `base-db.workspace = true`.

> applied by frontend-db-def: hir-expand and hir-def (tool file by file, plus the hand items;
> report `/tmp/blaze-lanes/v2-frontend-db-def.md`). `TestDBExt`'s two logging helpers are
> `log_in_read_order` and `executed_in_read_order`, not `log`/`log_executed`: `Db`'s inherent
> methods of those names would shadow them, and the old helpers ran under `LeftToReaders`.
> `ExpandProcAttrMacros` is gone, with its arm in both `nameres/collector.rs` and
> `nameres/facts/collect.rs`, and load-cargo's call. hir-ty-borrowck and hir-ty-wfcheck are
> not frontend-db-def's.

> applied by frontend-db-tyx (hir-ty-borrowck, hir-ty-wfcheck): the tool on both (40 and 25
> sites); each test database is `pub(crate) type TestDB = base_db::Db;` with `use hir_ty_mir as _;`
> for the MIR providers, so their `hir-ide`/`span`/`triomphe` (and wfcheck's `syntax`)
> dev-dependencies went, wfcheck's for `hir-ty-mir`. wfcheck already had `base-db`. Two comments
> in `regions.rs` that the tool rewrote (they name salsa's `dyn Database` in a user's program) were
> restored.

### 5. hir-ty: `HirDatabase` has one impl, and the MIR queries move in

> applied by frontend-db-ty: sealed trait with `NOT_DYN`, bodies in `impl HirDatabase for Db`,
> `crates/hir-ty/src/providers.rs` (24 memos keep their `X_query` names, cycle results, kinds
> 0x2490-0x24a7, `set_mir_capacity`/`set_runtime_mir_capacity` moved in), the tool on all of
> hir-ty. The `compile_fail` doc test is written but hir-ty has `doctest = false`
> (report `/tmp/blaze-lanes/v2-frontend-db-ty.md`).

- `crates/hir-ty/src/db.rs`: `pub trait HirDatabase: SourceDatabase + 'static` becomes
  `pub trait HirDatabase: sealed::Sealed` with `mod sealed { pub trait Sealed {} impl Sealed for base_db::Db {} }`
  and `#[doc(hidden)] const NOT_DYN: () = ();` (an associated const makes it not
  dyn-compatible). Delete `as_dyn`. Every provided method's body moves into
  `impl HirDatabase for base_db::Db` with `self` for `db` (`crate::lower::ty_query(self, def)`),
  so the trait holds signatures only. A caller's `db.ty(def)` is a static call to a body it can
  inline.
- The `hir_database_is_dyn_compatible` test (L498) becomes a `compile_fail` doc test on the
  trait: `fn f(_: &dyn hir_ty::db::HirDatabase) {}`.
- New `crates/hir-ty/src/providers.rs` (`pub mod providers;` in `lib.rs`):
  ```rust
  pub struct Mir {
      pub mir_body: for<'db> fn(&'db Db, InferBodyId<'db>) -> Result<Arc<MirBody<'db>>, MirLowerError<'db>>,
      // one field per query hir-ty-mir computes, in hir-ty-mir's signature and result types
      pub interpret_mir: for<'db> fn(&'db Db, &'db MirBody<'db>, bool, Option<ParamEnvAndCrate<'db>>) -> Result<Allocation<'db>, MirEvalError<'db>>,
      pub opaque_of_impl: for<'db> fn(&'db Db, ParamEnvAndCrate<'db>, InternedOpaqueTyId<'db>, GenericArgs<'db>) -> Option<Ty<'db>>,
  }
  pub struct Display {
      // `None` when the type has no source path to print; the caller makes that an error.
      pub type_name: for<'db> fn(&'db Db, Ty<'db>, ModuleId) -> Option<String>,
  }
  inventory::collect!(Mir);
  inventory::collect!(Display);
  pub fn mir() -> &'static Mir { static ONE: OnceLock<&'static Mir> = OnceLock::new(); ONE.get_or_init(|| one::<Mir>("hir-ty-mir")) }
  pub fn display() -> &'static Display { /* the same, "hir-ide" */ }
  fn one<T: inventory::Collect>(by: &str) -> &'static T {
      let mut all = inventory::iter::<T>();
      let first = all.next().unwrap_or_else(|| panic!("no `{}` is linked: link {by} (`use hir as _;`)", std::any::type_name::<T>()));
      assert!(all.next().is_none(), "two `{}` are linked", std::any::type_name::<T>());
      first
  }
  ```
  Then each MIR memo behind one of the trait's MIR methods (`mir_body_query`, the
  `_for_closure`, `_for_coroutine`, `_by_move`, `monomorphized_*`, `runtime_*` and
  `optimized_*` forms, plus `coroutine_witnesses` once db-ty's request adds it; not the
  `promoted_*` ones, which only crates above hir-ty-mir read and which stay there) is declared here, with its `#[salsa::tracked(...)]` attribute unchanged
  (`returns(as_ref)`, `lru`, `cycle_result`). Its cycle-result fn
  (`Err(MirLowerError::Loop)`) moves too. Its body is one call: `(mir().mir_body)(db, def)`.
  The trait's MIR methods call these. `interpret_mir`, `opaque_of_impl` and `type_name` call
  through the table directly.
- `crates/hir-ty/Cargo.toml`: `inventory.workspace = true`. The root `Cargo.toml` gets
  `inventory = "0.3.24"` under `[workspace.dependencies]` (the version salsa's lockfile entry
  is: nothing new is fetched); the root manifest is the coordinator's, so frontend-db asks
  there too.
- Run the tool on hir-ty (`method_resolution*` included: frontend-nameres-impls is told).

Why: hir-ty, hir-ty-borrowck and blaze-items read MIR, and its code is above them in
hir-ty-mir. Declaring the memo in hir-ty makes a hit static from every crate. Only a miss calls
through the table, as only a miss calls `execute` in rustc. hir-ty-mir registers the table at
link time (codegen's request), so no maker of a `Db` installs anything, and none can forget to.
A binary with no table, or two, panics on the first miss, naming what to link.

### 6. hir

Run the tool. `impl_hir_database` leaves the re-exports (L223). Add the test
`the_mir_and_display_providers_are_linked` in hir's tests: it asserts that
`hir_ty::providers::mir()` and `display()` return, in a binary that names nothing of hir-ty-mir.

> applied by frontend-db-hir: the tool run (652 sites); `Semantics<'db>` non-generic over `&'db Db` (`new_dyn` gone), `term_search`/`symbols` lost their `DB` parameter, the `impl_hir_database` and `set_expand_proc_attr_macros` re-exports gone, `check.rs`'s view downcast gone (`run_all` takes the closure by value), `tests/db.rs` is `type TestDB = base_db::Db;` (`executed` -> `log_executed`, `reverse_batches` -> `set_run_order(RunOrder::Reversed)`), and the test is in `src/tests.rs`.

### 7. hir-ty-matchck

Not in crates.md's table; db-matchck made it, so it goes to db. Run the tool on it (8 sites, 0
left).

> applied by frontend-db-tyx: the tool (12 sites, 0 left); the test database is
> `pub(crate) type TestDB = base_db::Db;` with `use hir_ty_mir as _;` (no `provide()`, no
> `set_expand_proc_attr_macros`), and the tests' `&*db` became `&db`.

> frontend-hooks (2026-10-29): nothing left to apply; §1-7 each have an applied answer above. The
> `compile_fail` test that doesn't run is its own section ("frontend-db: the `compile_fail` test
> ...").

## frontend-journal-tree: `Reader::file` is a `Result` of `Bytes` (blaze-query's `Tree`; 2026-10-29)

Why: restore.md, "The VFS tree itself is in the mapped image". A file of a restored tree is a
range of the daemon's mapped image, checked by its page group's CRC32C when it is first read.
`blaze_vfs::Reader::file` is now
`Result<Option<(ContentId, Mode, blaze_vfs::Bytes)>, blaze_vfs::Corrupt>`. `Bytes` is
`blaze_bytes::Bytes`, re-exported: a heap `Arc<[u8]>` or the image's range, `Deref<Target =
[u8]>`, and a clone takes one count. `Corrupt { section, group, cause }` names the bad group.
`crates/blaze-query/src/tree.rs:29-31` no longer compiles as written. Its `bytes.clone()` was
one `Arc` count; on a `Result` it is a type error.

What I'd do, but the shape is yours:
1. `Tree::file` -> `Result<Option<(ContentId, Mode, blaze_vfs::Bytes)>, blaze_vfs::Corrupt>`.
   For `blaze_vfs::Reader`: `blaze_vfs::Reader::file(self, root, path)`, with no clone, because
   it already gives owned `Bytes`. `MemTree`'s `Files` holds `Bytes`, built with
   `Bytes::from(Arc::<[u8]>::from(bytes))`, and its `file` is `Ok(..cloned())`.
2. `InputData::File` holds `Option<(ContentId, Mode, Bytes)>`. `Ctx::file` lends `&**bytes`, as
   it does now.
3. `read_input` on `Err(corrupt)`: insert nothing into `state.inputs`, and stop the run with
   the error, as a cancellation does, so the request fails naming the section and group.
   It must not be cached: the daemon mends the file by rescanning the path (the watcher
   writes the disk's bytes again, and `Tree::file` gives the node those bytes). The same root
   then reads fine, and a memoized error would outlive the repair. Treating it as `None`
   would be a fallback: the file would look absent, and the build would differ.

No error can happen before the daemon restores from an image (cargo's request), because until
then every file is a heap file.
Status: open

> forwarded to db (`blaze-query/src/tree.rs` is its; db-query-next2 told).
> frontend-hooks (2026-10-29): nothing to apply; already in (db-query). `Tree::file` returns
> `Result<Option<(ContentId, Mode, Bytes)>, Corrupt>` and `Reader`'s impl passes
> `blaze_vfs::Reader::file` through with no clone (`crates/blaze-query/src/tree.rs:18,29`).

## frontend-nameres-core-macros: `PerNsGlobImports::of_scope` (2026-10-29)

From `frontend-nameres-core-macros` (report `/tmp/blaze-lanes/v2-frontend-nameres-core-macros.md`),
for `crates/hir-def/src/nameres/facts/macros.rs`. One addition, nothing changed.

`crates/hir-def/src/item_scope.rs`, in `impl PerNsGlobImports {`, after `contains_macro`, add:
```rust
    /// The marks of `module`'s names, read off its scope: a name is marked in a namespace
    /// exactly when its entry there came from a glob (`push_res_with_import` and the
    /// collector's glob branch keep the two together).
    pub(crate) fn of_scope(module: ModuleId, scope: &ItemScope) -> PerNsGlobImports {
        let mut marks = PerNsGlobImports::default();
        for (name, item) in scope.types.iter() {
            if matches!(item.import, Some(ImportOrExternCrate::Glob(_))) {
                marks.types.insert((module, *name));
            }
        }
        for (name, item) in scope.values.iter() {
            if matches!(item.import, Some(ImportOrGlob::Glob(_))) {
                marks.values.insert((module, *name));
            }
        }
        for (name, item) in scope.macros.iter() {
            if matches!(item.import, Some(ImportOrExternCrate::Glob(_))) {
                marks.macros.insert((module, *name));
            }
        }
        marks
    }
```
Why: a macro step (`facts::macros::macro_step`) holds one module whose scope it clones from the
round's `ModuleScope` and applies its expansions' `DeclOp::Update`s to, as the old
`push_res_and_update_glob_vis` did; `push_res_with_import` needs the module's glob marks to let a
definition shadow a glob-imported name. The marks live in the unit's state, and reading that
state from a step would make every step of a component rerun when any member changes (the step
reads only its module's `fact_handle`, which backdates). Every entry whose `import` is a glob is
marked and no other: a glob write inserts the mark with the entry, a non-glob write that replaces
the entry removes it, and `update_def_*`/`update_visibility_*` keep the entry's import. The fields
are private to `item_scope.rs`, hence the hook.

Status: open

> forwarded to db (running; `db-next2` was told by the asker and again by requests).
> left for db-expand-names-next2 (frontend-hooks, 2026-10-29): owns `item_scope.rs` (running,
> touched 10 min before). Told by tmux.
> applied by db-expand-names (2026-10-29 ~21:00): `of_scope` is in, through mem-db-def-resolve R3
> (`of_scope_in(module, scope, Global)`, each set sized to its glob entries first; the same marks).
> Status: applied.

## dedup: frozen trees take LitKind's number from tt (from dedup-macros, 2026-10-29)

`crates/hir-def/src/nameres/frozen.rs` writes and reads `LitKind`'s number 0 to 10 with its own
two `match`es, equal to `crates/tt/src/storage.rs`'s. `tt::LitKind::code` and `from_code` are
requested of frontend-tokens (`v2/requests/frontend-tokens.md`, same heading). Apply this after
they land. Design: `docs/v2/plan/lanes/dedup/macros.md`.

1. The writer (around line 958, in the `Leaf::Literal` arm): replace
   `let (kind, hashes) = match kind { LitKind::Byte => (0, 0), ... LitKind::Err(()) => (10, 0), };`
   (eleven arms) with `let (kind, hashes) = kind.code();`.
2. The reader (around line 2289, the `1 =>` arm): replace
   `let kind = match kind { 0 => LitKind::Byte, ... 10 => LitKind::Err(()), it => return Err(format!("literal kind {it}")), };`
   with `let kind = LitKind::from_code(kind, hashes).ok_or_else(|| format!("literal kind {kind}"))?;`.
   The error is the same (`R` is `Result<_, String>`).

The bytes are unchanged, so no format version bump. Status: open

> forwarded to db (from requests, pass 16; told by tmux).

## dedup: `ProcMacroExpander` takes its call as one struct (proposal from dedup-macros, 2026-10-29)

The trait's eight parameters (`db, subtree, attrs, env, def_site, call_site, mixed_site,
current_dir`) are repeated, and forwarded, by every wrapping impl: `blaze_expand::Kept`
(`crates/blaze-expand/src/lib.rs` ~554-640), `blaze_macro_wasm::Expander` (`lib.rs` ~2128-2180),
`blaze-wasm`'s `Recorder` and `Replay` (`src/pack/record.rs`, `src/pack.rs`), `load-cargo`'s
`Expander`, and about 15 test expanders. The copy-paste scan reports these as the largest runs
left between those crates. A wrapper also has to clone `current_dir: String` to call its inner
expander twice (`Kept::expand` under `compare::enabled`).

Proposal, for whoever does frontend-db's `&Db` move of this trait, so each impl is touched once.
In `crates/hir-expand/src/proc_macro.rs`:

```rust
/// One call of a proc macro: what every expander is given.
#[derive(Clone, Copy, Debug)]
pub struct ProcMacroCall<'a> {
    pub subtree: &'a tt::TopSubtree,
    pub attrs: Option<&'a tt::TopSubtree>,
    pub env: &'a Env,
    pub def_site: Span,
    pub call_site: Span,
    pub mixed_site: Span,
    pub current_dir: &'a str,
}
```

`fn expand(&self, db: &Db, call: ProcMacroCall<'_>) -> Result<tt::TopSubtree, ProcMacroExpansionError>`
and `fn expand_in(&self, db: &Db, calling_crate: Crate, call: ProcMacroCall<'_>) -> ...`
(default: `self.expand(db, call)`). A wrapper forwards `call` as is. The one caller that builds
it (`impl CustomProcMacroExpander` in `hir-expand/src/proc_macro.rs`, ~391) passes
`calling_crate.data(db).proc_macro_cwd.as_str()` (an `Arc<AbsPathBuf>`; `as_str` is
`paths`') and drops its `to_string()` (the `FIXME: Can we avoid
the string allocation here?` there). An impl that needs an owned directory (the proc-macro-api client, for its
message) makes it there.

This is an interface change across lanes (usercode, cargo, coordinator's `blaze-wasm`, tests),
so it is a proposal. If db takes it, dedup-macros writes the exact per-impl edits as requests to
each owner. Status: open

> forwarded to db (from requests, pass 16; told by tmux).


## dedup: stdx::edit_distance

From `dedup-scan` (lane dedup; report `/tmp/blaze-lanes/v2-dedup-scan.md`). `crates/hir-expand/src/builtin/fn_macro.rs`
1400-1448 is one of six copies of rustc's `edit_distance`; the one copy is `stdx::edit_distance`
(`v2/requests/coordinator.md`, `## dedup: one stdx::edit_distance`, with a proptest differential
against rustc's code). `stdx` is already a dependency of `hir-expand`. Apply once that lands:

- Delete lines 1400-1448 (the doc comment and `fn edit_distance`).
- Add `use stdx::edit_distance::edit_distance;` to the file's imports.
- `similar_cargo_var` (rustc's `find_similar_cargo_var`, with its `PACKAGE`/`PKG` rule) stays: it
  is not `find_best_match_for_name`.

Status: open

> forwarded to db (from requests, pass 16; told by tmux).


## dedup: `hir_def::test_items`, a fixture's items by name for the tests above hir-def

From `dedup-scan` (lane dedup; report `/tmp/blaze-lanes/v2-dedup-scan.md`). Six test modules
wrote the same `declarations(db)` (every `ModuleDefId` of a crate's modules) and the same
"the one function/type alias/static/constant named `x`" lookups:
`hir-ty-codegen/v2/hashes/tests.rs` 34-72, `hir-ty-codegen/v2/refs/tests.rs` 92-128,
`hir-ty-codegen/v2/tests.rs` 123-138, `hir-ty-mir/tests/mir/main.rs` 25-66,
`hir-ty-share/src/tests/mod.rs` 124-161, `blaze-items/src/tests/{narrow,coherence}.rs`
(`declarations`, `impls`). They need `crate_def_map`, so the one copy is in hir-def (not
base-db). It is compiled always, not under `cfg(test)`: the tests that use it are other
crates' (a `cfg(test)` module of hir-def is invisible to them), and it is 60 lines. With the test
databases becoming `type TestDB = base_db::Db;` (frontend-db), it takes `&Db`. The callers'
edits: `v2/requests/codegen.md`, `v2/requests/fastcompile-ident-share.md`, `v2/requests/items.md`,
each under `## dedup: hir_def::test_items`.

> forwarded to db (from requests, pass 16; told by tmux).

### 1. `crates/hir-def/src/lib.rs`: after `pub mod visibility;` (line 49) add

```rust
pub mod test_items;
```

> forwarded to db (from requests, pass 16; told by tmux).

### 2. New file `crates/hir-def/src/test_items.rs`

```rust
//! A fixture's items found by name, for the tests of the crates above hir-def: the lookups each
//! of their test modules wrote again.

use base_db::{Crate, Db};

use crate::{
    ConstId, FunctionId, ImplId, ModuleDefId, StaticId, TypeAliasId,
    nameres::crate_def_map,
    signatures::{ConstSignature, FunctionSignature, StaticSignature, TypeAliasSignature},
};

/// Every item declared in `krate`'s modules, module by module, each in the order written.
pub fn declarations(db: &Db, krate: Crate) -> Vec<ModuleDefId> {
    crate_def_map(db, krate)
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).declarations().collect::<Vec<_>>())
        .collect()
}

/// Every impl in `krate`'s modules, module by module, each in the order written.
pub fn impls(db: &Db, krate: Crate) -> Vec<ImplId> {
    crate_def_map(db, krate)
        .modules(db)
        .flat_map(|(_, module)| module.scope(db).impls().collect::<Vec<_>>())
        .collect()
}

/// The one function named `name` in `krate`; panics with how many there are otherwise.
#[track_caller]
pub fn function(db: &Db, krate: Crate, name: &str) -> FunctionId {
    one(db, krate, "functions", name, |def| match def {
        ModuleDefId::FunctionId(it) if FunctionSignature::of(db, it).name.as_str() == name => Some(it),
        _ => None,
    })
}

/// The one type alias named `name` in `krate`; panics with how many there are otherwise.
#[track_caller]
pub fn type_alias(db: &Db, krate: Crate, name: &str) -> TypeAliasId {
    one(db, krate, "type aliases", name, |def| match def {
        ModuleDefId::TypeAliasId(it) if TypeAliasSignature::of(db, it).name.as_str() == name => Some(it),
        _ => None,
    })
}

/// The one static named `name` in `krate`; panics with how many there are otherwise.
#[track_caller]
pub fn static_item(db: &Db, krate: Crate, name: &str) -> StaticId {
    one(db, krate, "statics", name, |def| match def {
        ModuleDefId::StaticId(it) if StaticSignature::of(db, it).name.as_str() == name => Some(it),
        _ => None,
    })
}

/// The one named constant `name` in `krate`; panics with how many there are otherwise.
#[track_caller]
pub fn constant(db: &Db, krate: Crate, name: &str) -> ConstId {
    one(db, krate, "constants", name, |def| match def {
        ModuleDefId::ConstId(it)
            if ConstSignature::of(db, it).name.as_ref().is_some_and(|it| it.as_str() == name) =>
        {
            Some(it)
        }
        _ => None,
    })
}

#[track_caller]
fn one<T: Copy>(db: &Db, krate: Crate, what: &str, name: &str, pick: impl FnMut(ModuleDefId) -> Option<T>) -> T {
    let found: Vec<T> = declarations(db, krate).into_iter().filter_map(pick).collect();
    match found[..] {
        [one] => one,
        _ => panic!("{} {what} named `{name}`", found.len()),
    }
}
```

Its tests are its callers' (every helper is used by several suites above); hir-def's own
`test_db.rs` can call `test_items::function(&db, db.test_crate(), "f")` wherever it looks one up.

Status: open

> forwarded to db (from requests, pass 16; told by tmux).

> applied by db-apply-def (21:12): `hir-def/src/test_items.rs` as written and `pub mod test_items;` after `pub mod visibility;` in `lib.rs`; one difference: `declarations`/`impls` flat_map the scope iterators directly instead of collecting each module's into a `Vec` first (same order, one allocation).

## dedup: salsa's `Sip` hasher forwarders onto `blaze_ids::fixed_width_writes!`

From `dedup-codec` (report `/tmp/blaze-lanes/v2-dedup-codec.md`). `vendor/salsa/src/fingerprint.rs`
`impl Hasher for Sip<C, D>` (about lines 216-262) hand-writes `write_u8`..`write_isize` as
little-endian bytes through `self.write`, `usize`/`isize` as 64 bits; so did `hir-ty-codegen`,
`hir-ty-share` and `blaze-hashcons`. The one copy is the macro `blaze_ids::fixed_width_writes!()`
in `crates/blaze-ids/src/std_hash.rs` (the same bytes; tested against the hand-written form).

1. `vendor/salsa/Cargo.toml`, a dependency (the manifest is in cargo's normalized form):

       [dependencies.blaze-ids]
       path = "../../crates/blaze-ids"

   (`blaze-ids` depends on `blake3`, `blaze-radix` and `serde` only, none of them on salsa.)

2. `vendor/salsa/src/fingerprint.rs`, in `impl<const C: usize, const D: usize> Hasher for Sip<C, D>`:
   keep `write` and `finish`; replace the twelve methods `write_u8` through `write_isize` by the
   one line `blaze_ids::fixed_width_writes!();`. (`write_u8` becomes `self.write(&[i])`, as now;
   `write_isize` becomes `(i as i64).to_le_bytes()`, the bytes of `i as i64 as u64` now.)

> forwarded to db (from requests, pass 16; told by tmux).

> Applied by dedup-codec (2026-10-29, 21:15): the file was free (no live agent held it). The
> `[dev-dependencies.blaze-ids]` line dedup-codec-rng added became the `[dependencies]` one.


## speed-hir-store-paths: paths as columns (2026-10-29)

From `speed-hir-store-paths-consumers` (child of `speed-hir-store-paths`). hir-def's paths and
generic arguments are columns now (`crates/hir-def/src/expr_store/path.rs`; the API is in
`/tmp/blaze-lanes/v2-speed-hir-store-paths.md`, "The API"): `Path<'a>`, `GenericArgs<'a>`,
`AssociatedTypeBinding<'a>`, `PathGenericArgs<'a>` and `Bindings<'a>` are `Copy` views decoded
from the store; `TypeBound` and `WherePredicate` are `Copy` records whose `for<..>` binders and
`use<..>` arguments are runs (`Names`, `UseArgs`) read with `store.names(..)` and
`store.use_args(..)`. Apply these with the paths lane's own edits to `hir.rs`/`expr_store*`
(one compile). Nothing here is built (common.md).

The consumer rule:

1. `&Path` parameters and fields become `Path<'_>` (by value; `Copy`). Where a function keeps
   `&Path<'_>`, callers pass `&path`. `&store[p]` / `&map[p]` for a `PathId` -> `store.path(p)`.
2. `Path::Normal(it) => &it.mod_path` -> `it.mod_path` (already a reference).
   `it.generic_args` / `it.type_anchor` field reads are unchanged.
3. `generic_args()` yields slots by value: `.into_iter().flatten().flatten()` still gives
   `GenericArgs<'_>`; `.iter().any(Option::is_some)` -> `.iter().any(|it| it.is_some())`;
   `generic_args()?.last()?.as_ref()?` -> `generic_args()?.last()??`; `&generic_args[i]` ->
   `generic_args.get(i)`.
4. `&GenericArgs` parameters -> `GenericArgs<'_>`; `Option<&GenericArgs>` -> `Option<GenericArgs<'_>>`
   (drop `.as_ref()`/`.as_deref()`; `*args` -> `args`). `args.bindings.iter()`, `for b in
   &args.bindings`, `.first()`, `.is_empty()`, `.len()` keep working; `bindings[i]` ->
   `bindings.get(i).unwrap()` (or restructure), `&*bindings`/`bindings.as_ref()` -> iterate.
5. `binding.args.as_ref()` -> `binding.args`; `&binding.bounds` / `binding.bounds.iter()`
   unchanged (`&'a [TypeBound]`).
6. `TypeBound::ForLifetime(lifetimes, path)`: `lifetimes` is `Names`; read with
   `store.names(lifetimes)`. `TypeBound::Use(args)`: `store.use_args(args)`. `bound.clone()` ->
   `*bound`; `&TypeBound::Path(p, m)` patterns keep working.
7. `WherePredicate::TypeBound { lifetimes: Some(l), .. }`: `l` is `Names`; `store.names(l)`.
8. `Path::from_known_path_with_no_generic(m)` -> `Path::from_known_path_with_no_generic(&m)`
   (bind the `ModPath` to a local if it is a temporary used past the statement).
9. `path.qualifier()` gives `Qualifier<'_>`: use `.path()` for a `Path<'_>` (bind the
   qualifier to a local first: `let q = path.qualifier()?; let q = q.path();`).

Applied the same way everywhere, so that callers in one crate agree with signatures in another:
every `&Path`, `Option<&GenericArgs>`, `&GenericArgs`, `&[Option<GenericArgs>]` parameter,
field and return becomes the by-value view, hir-def's `Resolver::resolve_path_in_*` and
`record_path_uses` and hir-ty's path lowering included. Bodies are by-value views
(`store.expr(id) -> Expr<'_>`), so `Expr::Path(p)` binds `p: Path<'_>` and most call sites
keep their text. Also `args.args` is `&[GenericArg]` now, so `for arg in &args.args` becomes
`for arg in args.args` (`&&[T]` is not `IntoIterator`).

The edits assume, from the paths lane: `Expr::Path`/`Pat::Path`/`RecordLit`/`Pat::Record`/
`Pat::TupleStruct` hold `Path<'a>`; `Expr::MethodCall { generic_args: Option<GenericArgs<'a>> }`;
`AsmOperand::Sym(PathIdx)` read with `store.path_at(idx)` (both `pub`); `Index<PathId> for
ExpressionStore` removed; `StoreVisitor::on_path`/`on_generic_args`/`on_type_bound` taking the
views by value; `ExprCollector::lower_path -> Option<PathIdx>`.

Line numbers are as of 20:50; several of these files are being edited, so find each edit by its
old text (where the same text is on several lines, the function named beside it picks).

> forwarded to db (from requests, pass 16; told by tmux).


### crates/hir-def/src/resolver.rs

`path: &Path` becomes `path: Path<'_>` in all seven methods that take a path
(`resolve_path_in_type_ns`, `resolve_path_in_type_ns_with_prefix_info`,
`resolve_path_in_type_ns_fully`, `resolve_path_in_value_ns`,
`resolve_path_in_value_ns_with_prefix_info`, `resolve_path_in_value_ns_fully`,
`record_path_uses`); the inner calls pass `path` on unchanged (`Path` is `Copy`). In the
`match path` that opens three of them, `Path::LangItem(l, ..)` binds `l: LangItemTarget` by
value now, so `match *l` becomes `match l`.

- line 211: `        path: &Path,` -> `        path: Path<'_>,`
- line 229: `        path: &Path,` -> `        path: Path<'_>,`
- line 239: `            Path::Normal(it) => &it.mod_path,` -> `            Path::Normal(it) => it.mod_path,`
- line 241: `                let type_ns = match *l {` -> `                let type_ns = match l {`
- line 341: `        path: &Path,` -> `        path: Path<'_>,`
- line 379: `        path: &Path,` -> `        path: Path<'_>,`
- line 399: `        path: &Path,` -> `        path: Path<'_>,`
- line 404: `            Path::Normal(it) => &it.mod_path,` -> `            Path::Normal(it) => it.mod_path,`
- line 407: `                    ResolveValueResult::ValueNs(match *l {` -> `                    ResolveValueResult::ValueNs(match l {`
- line 425: `                let type_ns = match *l {` -> `                let type_ns = match l {`
- line 591: `        path: &Path,` -> `        path: Path<'_>,`
- line 638: `        path: &Path,` -> `        path: Path<'_>,` (`record_path_uses`)
- line 645: `            Path::Normal(it) => &it.mod_path,` -> `            Path::Normal(it) => it.mod_path,`
- line 871: `.resolve_path_in_type_ns_fully(db, &impl_data.store[target_trait.path])` -> `.resolve_path_in_type_ns_fully(db, impl_data.store.path(target_trait.path))`

> forwarded to db (from requests, pass 16; told by tmux).

### crates/hir-def/src/upvars.rs

- line 151: `    fn resolve_maybe_upvar(&mut self, expr: ExprId, path: &Path) {` -> `    fn resolve_maybe_upvar(&mut self, expr: ExprId, path: Path<'_>) {`
- lines 206-208 (`Expr::Path(path) => { self.resolve_maybe_upvar(expr, path); self.on_path(path); }`)
  stay as they are: `path` is `Path<'_>` from the by-value view. This needs
  `StoreVisitor::on_path` (`expr_store.rs`, the paths lane's) to take `path: Path<'_>` by
  value, per the signature rule.

> forwarded to db (from requests, pass 16; told by tmux).

### crates/hir-def/src/nameres/uses.rs

- line 557: `resolver.record_path_uses(db, &path, Namespace::Types, hygiene, out);` -> `resolver.record_path_uses(db, path, Namespace::Types, hygiene, out);`
- lines 535-551 stay: `Expr::Path(path)`, `Expr::RecordLit { path, .. }`, `Pat::Path(path)`,
  `Pat::TupleStruct { path, .. }`, `Pat::Record { path, .. }` bind `Path<'_>` from the
  by-value `store.exprs()`/`store.pats()` and pass it on as is.

> forwarded to db (from requests, pass 16; told by tmux).


### crates/hir-ty/src/lower/path.rs

- line 63: `    path: &'a Path,` -> `    path: Path<'a>,`
- line 77: `        path: &'a Path,` -> `        path: Path<'a>,`
- line 762: `        args_and_bindings: Option<&HirGenericArgs>,` -> `        args_and_bindings: Option<HirGenericArgs<'_>>,`
- line 1023: `                            binding.args.as_ref(),` -> `                            binding.args,`
- line 1125: `    Path(&'a Path),` -> `    Path(Path<'a>),`
- line 1196: `    args_and_bindings: Option<&HirGenericArgs>,` -> `    args_and_bindings: Option<HirGenericArgs<'_>>,`
- line 1312: `    args_and_bindings: Option<&HirGenericArgs>,` -> `    args_and_bindings: Option<HirGenericArgs<'_>>,`
- line 1330: `args_and_bindings.map(|it| &*it.args)` -> `args_and_bindings.map(|it| it.args)`

Sites that keep compiling as written: `self.path` passed to the resolver (`:349`, `:352`, `:405`,
by value under the rule) and `matches!(self.path, Path::LangItem(..))`; every
`.args_and_bindings.is_some()/is_none()`; `:708` `if let Some(args) = ..args_and_bindings`
(a `Copy` value now); `:989`-`:990` `args_and_bindings.bindings.iter().enumerate()` in the
`move` closure (yields `AssociatedTypeBinding<'a>`; `binding.name.clone()`, `binding.type_ref`,
`binding.bounds.iter()` unchanged); `:1186` `&args_and_bindings.args[..]`; `:1319`
`is_some_and(|it| it.has_self_type)`; `:1550` `TypeRef::Path(path) => TypeLikeConst::Path(path)`
(`store.type_ref` is by value).

> forwarded to db (from requests, pass 16; told by tmux).

### crates/hir-ty/src/lower.rs

- line 818: `pub(crate) fn lower_path_as_const(&mut self, path: &Path, _const_type: Ty<'db>)` -> `pub(crate) fn lower_path_as_const(&mut self, path: Path<'_>, _const_type: Ty<'db>)`
- line 1221: `            &self.store[path_id],` -> `            self.store.path(path_id),`
- line 1226: `pub(crate) fn lower_path(&mut self, path: &Path, path_id: PathId)` -> `pub(crate) fn lower_path(&mut self, path: Path<'_>, path_id: PathId)`
- line 1252: `pub fn lower_value_path_qualifier(&mut self, path: &Path) -> Option<Ty<'db>> {` -> `pub fn lower_value_path_qualifier(&mut self, path: Path<'_>) -> Option<Ty<'db>> {`
- line 1260: `PathLoweringContext::new(self, on_diagnostic, &qualifier);` -> `PathLoweringContext::new(self, on_diagnostic, qualifier.path());`
- line 1315: `self.with_shifted_in(lifetimes, |ctx| lower_type_outlives(ctx, target, bound)).0` -> `self.with_shifted_in(self.store.names(*lifetimes), |ctx| lower_type_outlives(ctx, target, bound)).0`
  (`lifetimes: &Names`; `ctx.store` is the predicate's store: callers `set_owner` to the
  generics whose predicates they lower, `:2808`, `:3556`, traits.rs `:417`)
- lines 1360-1364 (the `for<..>` bound of an associated type's bound):

```old
            &TypeBound::ForLifetime(ref binder, path)
                if of_assoc_ty
                    && self.bound_vars_with_elided().len() == self.peek_bound_vars().len() =>
            {
                let generic_def = self.generic_def;
```

```new
            &TypeBound::ForLifetime(binder, path)
                if of_assoc_ty
                    && self.bound_vars_with_elided().len() == self.peek_bound_vars().len() =>
            {
                let binder = self.store.names(binder);
                let generic_def = self.generic_def;
```

  (`binder` borrows `*self.store`, not `self`, so `self.bound_vars.last_mut()` after it is fine;
  `binder.iter()` at `:1367`-`:1368` reads the slice unchanged)
- lines 1376-1377:

```old
            &TypeBound::ForLifetime(ref binder, path) => {
                self.with_shifted_in(binder, |ctx| {
```

```new
            &TypeBound::ForLifetime(binder, path) => {
                self.with_shifted_in(self.store.names(binder), |ctx| {
```

- line 2754: `resolver.resolve_path_in_type_ns_fully(db, &signature.store[bounded_trait])` -> `resolver.resolve_path_in_type_ns_fully(db, signature.store.path(bounded_trait))`
- line 4159: `path_lifetimes(db, resolver, store, &store[*path], found, self_references)?` -> `path_lifetimes(db, resolver, store, store.path(*path), found, self_references)?`
- line 4175: `    path: &Path,` -> `    path: Path<'_>,`

Keep compiling as written: `:880`-`:882` and `:4109` (`TypeRef::Path(path)` from the by-value
`store.type_ref`, passed on by value); `:1137`-`:1147` (`path` by value to
`resolve_path_in_type_ns`); `:2692`-`:2709`, `:2810`-`:2827`, `:3492`-`:3501` (`TypeBound::Path`
/ `ForLifetime(_, p)` patterns on `*bound`, `PathId`s); `:4155`
`path.generic_args().into_iter().flatten().flatten()` (gives `GenericArgs<'_>`; `it.bindings
.is_empty()`, `it.parenthesized`, `it.args.iter()` unchanged); `:1100` (`FnType::binder` is
unchanged).

> forwarded to db (from requests, pass 16; told by tmux).

### crates/hir-ty/src/consteval.rs

- line 319: `    path: &Path,` -> `    path: Path<'_>,`

`:393` `Expr::Path(path)` from `match expr` (`store.expr` by value) binds `Path<'_>` and passes
it as is.

> forwarded to db (from requests, pass 16; told by tmux).

### crates/hir-ty/src/mir.rs

- line 14: `    expr_store::{ExpressionStore, path::Path},` -> `    expr_store::ExpressionStore,`
- line 17: `use hir_expand::name::Name;` -> `use hir_expand::{mod_path::ModPath, name::Name};`
- line 2180:

```old
        path: Path,
    },
    RecordLiteralWithoutPath,
```

```new
        /// `None` for a lang item path.
        path: Option<ModPath>,
    },
    RecordLiteralWithoutPath,
```

  (`ModPath` has no lifetime, so `derive(SalsaValue)` accepts it as the old `Path` was; the
  constructors in hir-ty-mir are under Cross-part)

`mir.rs`'s `AsmOperand::Sym` hits are MIR's own `AsmOperand`, not hir-def's.

> forwarded to db (from requests, pass 16; told by tmux).

### Checked, nothing breaks

- `crates/hir-ty/src/generics.rs:194`-`:210`: `TypeRef::Path(path)` by value; `type_anchor()`,
  `segments()`, `.args_and_bindings.is_none()`, `mod_path().and_then(|it| it.as_ident())` all
  keep their types; the `&WherePredicate::TypeBound { lifetimes: None, .. }` pattern works with
  `Option<Names>`.
- `crates/hir-ty/src/effects.rs:285`: `Expr::MethodCall { .. }`.
- `crates/hir-ty/src/db.rs:759`: `matches!(store.expr(..), Expr::Const(_))`.
- False hits only (rustc_type_ir's `GenericArgs`/`GenericArg`, diagnostic enum names
  `GenericArgsProhibited`, `ParenthesizedGenericArgsWithoutFnTrait`): `builtin_derive.rs`,
  `dyn_compatibility.rs`, `inhabitedness.rs`, `intrinsic.rs`, `lib.rs`, `lower/batch.rs`,
  `lower/diagnostics.rs`, `lower/persist.rs`, `representability.rs`, `solver_errors.rs`,
  `variance.rs`, `next_solver/*` (`binder.rs`, `consts.rs`, `generic_arg.rs`, `interner.rs`,
  `ir_print.rs`, `persist.rs`, `solver.rs`, `ty.rs`, `util.rs`, `infer/*`, `predicate.rs`,
  `inspect.rs`, `next_solver.rs`). The rest of `crates/hir-ty/src/` outside `infer/`,
  `infer.rs` and the items part's files (`mir/`, `variance/`, `target_feature/`, `autoderef.rs`,
  `utils.rs`, `primitive.rs`, `lang_items.rs`, `evict.rs`) has no hit.

> forwarded to db (from requests, pass 16; told by tmux).


### crates/hir-ty/src/infer.rs

- line 2435 (`make_path_as_const`): `pub(crate) fn make_path_as_const(&mut self, path: &Path) -> Const<'db> {` -> `pub(crate) fn make_path_as_const(&mut self, path: Path<'_>) -> Const<'db> {`
  (the `path_to_const(.., path)` call in its body stays: `path_to_const` takes `Path<'_>`, see
  the `lower` part).
- line 2580 (`fn resolve_variant`, the parameter after `node: ExprOrPatIdPacked,`): `        path: &Path,` -> `        path: Path<'_>,`
  (body stays: `path.type_anchor()`, `path.segments()`, `path.mod_path()` and
  `ctx.at_path(path, node)` all take the value).

> forwarded to db (from requests, pass 16; told by tmux).

### crates/hir-ty/src/infer/diagnostics.rs

- line 138 (`at_path`): `        path: &'b Path,` -> `        path: Path<'b>,`
- line 155 (`at_path_forget_diagnostics`): `        path: &'b Path,` -> `        path: Path<'b>,`
  (both `PathLoweringContext::new(&mut self.ctx, on_diagnostic, path)` calls stay; `new` takes
  `Path<'a>`.)

> forwarded to db (from requests, pass 16; told by tmux).

### crates/hir-ty/src/infer/path.rs

- line 33 (`infer_path`): `        path: &Path,` -> `        path: Path<'_>,`
- line 64 (`resolve_value_path`): `        path: &Path,` -> `        path: Path<'_>,`
- line 149 (`resolve_value_path_inner`): `        path: &Path,` -> `        path: Path<'_>,`
  (bodies stay: `if let Path::LangItem(..) = path` and `matches!(path, Path::LangItem(..))`
  match the value; `ctx.at_path(path, id)`, `ctx.at_path_forget_diagnostics(path)` and the
  `with_ty_lowering(|ctx| .. ctx.at_path(path, id) ..)` closure take the `Copy` value.)

> forwarded to db (from requests, pass 16; told by tmux).

### crates/hir-ty/src/infer/expr.rs

- line 930: `AsmOperand::Sym(ref path) => {` -> `AsmOperand::Sym(path) => {`
- line 934: `self.infer_path(path, tgt_expr.into())` -> `self.infer_path(self.store.path_at(path), tgt_expr.into())`
  (`AsmOperand::Sym` holds a `PathIdx`; `self.store` is `&'db ExpressionStore`, so the view
  borrows the store, not `self`.)
- line 1053 (`infer_record_expr`): `        path: &Path,` -> `        path: Path<'_>,`
- line 1360: `fn infer_expr_path(&mut self, path: &Path, id: ExprOrPatIdPacked, scope_id: ExprId) -> Ty<'db> {` -> `fn infer_expr_path(&mut self, path: Path<'_>, id: ExprOrPatIdPacked, scope_id: ExprId) -> Ty<'db> {`
- line 1905 (`infer_method_call`): `        generic_args: Option<&HirGenericArgs>,` -> `        generic_args: Option<HirGenericArgs<'_>>,`

Stay as written: `Expr::Path(p) => self.infer_expr_path(p, ..)` (521), `Expr::RecordLit { path,
.. } => self.infer_record_expr(.., path, ..)` (648), `Expr::MethodCall { .., generic_args } =>
self.infer_method_call(.., generic_args, ..)` (450), `is_syntactic_place_expr`'s
`Expr::Path(Path::Normal(path)) => path.type_anchor.is_none()` and
`resolve_path_in_value_ns_fully(self.db, path, ..)` (225-237; `match self.store.expr(expr)` is
by value), and `lookup_method_including_private(.., None, ..)` (1768).

> forwarded to db (from requests, pass 16; told by tmux).

### crates/hir-ty/src/infer/pat.rs

- line 984: `fn resolve_record_pat(&mut self, pat: PatId, path: &Path) -> Result<ResolvedPat<'db>, ()> {` -> `fn resolve_record_pat(&mut self, pat: PatId, path: Path<'_>) -> Result<ResolvedPat<'db>, ()> {`
- line 1011: `fn resolve_pat_path(&mut self, pat: PatId, path: &Path) -> Result<ResolvedPat<'db>, ()> {` -> `fn resolve_pat_path(&mut self, pat: PatId, path: Path<'_>) -> Result<ResolvedPat<'db>, ()> {`
- line 1041 (`resolve_tuple_struct_pat`): `        path: &Path,` -> `        path: Path<'_>,`
  (the callers at 308-310 match `self.store.pat(pat_id)` by value and pass `path` as is.)

> forwarded to db (from requests, pass 16; told by tmux).

### No edits

- `infer/closure/analysis.rs:331` and `infer/closure/analysis/expr_use_visitor.rs:950, 1344`:
  `Expr::Path(path)` / `Pat::Path(path)` of by-value `store.expr`/`store.pat` passed to
  `resolve_path_in_value_ns_fully`, which takes `Path<'_>`.
- `expr_use_visitor.rs:605` `AsmOperand::Sym { .. }`, `infer/mutability.rs:83, 120, 145, 201`:
  patterns that ignore the field.
- `infer/persist.rs`, `unify.rs`, `callee.rs`, `op.rs`, `place_op.rs`, `coerce.rs`,
  `closure.rs`: only `next_solver` `GenericArgs`/`GenericArg` and `MethodCallee`/diagnostic
  names (false hits).

> forwarded to db (from requests, pass 16; told by tmux).


### crates/hir-ty-matchck/src/lower.rs

- line 320: `path: &Path,` -> `path: Path<'_>,`

(Callers at lines 192, 340 and 578 bind `path` by value from `self.body.pat(..)` /
`self.body.expr(..)` and pass it as is; line 326 passes it to `resolve_path_in_value_ns_fully`,
which takes `Path<'_>`.)

> forwarded to db (from requests, pass 16; told by tmux).

### crates/hir-ty-unsafeck/src/lib.rs

- line 900: `fn check_static(&mut self, current: ExprId, path: &Path) {` -> `fn check_static(&mut self, current: ExprId, path: Path<'_>) {`

(Line 389 passes `path` bound by value from `store.expr(current)`, where `store` is a copy of
`self.store`, so the `&mut self` call doesn't conflict; line 912 passes it to
`resolve_path_in_value_ns` as is.)

> forwarded to db (from requests, pass 16; told by tmux).

### crates/hir-ty-wfcheck/src/lib.rs

- line 594: `store[*path].segments()` -> `store.path(*path).segments()`

(`compare.rs`, `outlives.rs`, `outlives/tests.rs`: only `next_solver` hits, no edits.)

> forwarded to db (from requests, pass 16; told by tmux).

### crates/hir-ty-borrowck/src/regions.rs

- line 330 (`opaque_captures`; `args` is now a `UseArgs` run with no `iter`; `listed` is read
  with `.iter()` at lines 389 and 429, which keep working on the slice): `TypeBound::Use(args) => Some(args),` -> `TypeBound::Use(args) => Some(signature.store.use_args(*args)),`
- line 474 (`nth_impl_trait`): `let in_path = |path: &'s hir_def::expr_store::path::Path, index: &mut usize| {` -> `let in_path = |path: hir_def::expr_store::path::Path<'s>, index: &mut usize| {`
- line 475 (`args.args` is `&'s [GenericArg]` now; `&&[T]` is not `IntoIterator`): first of the two
  `for arg in &args.args {` -> `for arg in args.args {`
- line 496: `in_path(&store[*path], index)` -> `in_path(store.path(*path), index)`
- lines 4183 (`written_self`), 4396 (`spelled_positions`), 4444 (`spelled_projections`), 4643
  (`lower_written_qualifier`), 6772 (`names_through_type`), all five occurrences:
  `path: &hir_def::expr_store::path::Path,` -> `path: hir_def::expr_store::path::Path<'_>,`
- line 4420 (`spelled_positions`; `args` is `Option<GenericArgs<'_>>`, consumer rule 4): `.as_ref()?` -> `?`
- line 6615 (`writes_self`): `fn args(store: &hir_def::expr_store::ExpressionStore, generic: &GenericArgs) -> bool {` -> `fn args(store: &hir_def::expr_store::ExpressionStore, generic: GenericArgs<'_>) -> bool {`
- line 6622: `binding.args.as_ref().is_some_and(|it| args(store, it))` -> `binding.args.is_some_and(|it| args(store, it))`
- line 6626: `fn path(store: &hir_def::expr_store::ExpressionStore, path: &Path) -> bool {` -> `fn path(store: &hir_def::expr_store::ExpressionStore, path: Path<'_>) -> bool {`
- line 6660 (`writes_elided`): `let path = |path: &Path| {` -> `let path = |path: Path<'_>| {`
- line 6679: `TypeBound::Path(it, _) => path(&store[*it]),` -> `TypeBound::Path(it, _) => path(store.path(*it)),`
- line 6726 (`elided_lifetimes`): second of the two `for arg in &args.args {` -> `for arg in args.args {`
- line 6852: `pub(super) fn spells_generics(path: &hir_def::expr_store::path::Path) -> bool {` -> `pub(super) fn spells_generics(path: hir_def::expr_store::path::Path<'_>) -> bool {`
- line 6853 (`iter` yields `Option<GenericArgs<'_>>` by value; `Option::is_some` takes `&self`): `args.iter().any(Option::is_some)` -> `args.iter().any(|it| it.is_some())`

Sites that keep compiling once those signatures change (checked, no edit): every
`Expr::Path(path)` / `Pat::Path(path)` / `Expr::RecordLit { path, .. }` bound by value from
`store.expr(..)` / `store.pat(..)` and passed to `spells_generics`, `written_self`,
`spelled_projections`, `spelled_positions`, `names_through_type` or the resolver (lines ~2106,
2113, 2233, 2360, 3883–3897, 4120–4142, 4958, 4999, 5047–5049); `path.generic_args()
.into_iter().flatten().flatten()` (lines 474, 2118, 6629, 6660, 6722); `args.bindings.iter()`,
`&args.bindings`, `args.bindings.is_empty()`; `qualifier.args_and_bindings?` (line ~4648,
now `Option<GenericArgs>` by value); `path.qualifier().is_some_and(|it| it.is_self_type())`
(`Qualifier::is_self_type` exists); `bound.as_path(store)` (line ~6632, gives `Path<'_>`);
`TypeRef::Path(path)` from `store.type_ref(..)` (by value, lines 509, 5419, 6635, 6671, 6714).

> forwarded to db (from requests, pass 16; told by tmux).

### crates/hir-ty-borrowck/src/scan.rs, scan/tests.rs, loans.rs, qualifs.rs

No edits: `Pat::Record { path, .. } | Pat::TupleStruct { path, .. } | Pat::Path(path)` and
`Expr::Path(path)` bind `Path<'_>` by value (`spelled(pat: Pat<'_>)`, `store.exprs()`,
`store.expr(..)`), passed to `spells_generics` (now `Path<'_>`) or
`resolve_path_in_value_ns_fully` as is; `path.is_self_type()` (loans.rs 208) works on the view.

> forwarded to db (from requests, pass 16; told by tmux).

### crates/hir-ty-borrowck/src/check.rs, lib.rs, liveness.rs, names.rs, polonius.rs, moves.rs

No edits: `next_solver` `GenericArgs`, MIR `AsmOperand`, and `self.bindings` / `store[binding]`
(binding ids, not paths).

> forwarded to db (from requests, pass 16; told by tmux).

### crates/hir-ty-matchck/src/cx.rs

No edits: `next_solver` `GenericArgs` only.

> forwarded to db (from requests, pass 16; told by tmux).


### crates/hir/src/attrs.rs

```old
    let path = Path::from_known_path_with_no_generic(path);
    let base_def = resolver.resolve_path_in_type_ns_fully(db, &path)?;
```
```new
    let path = Path::from_known_path_with_no_generic(&path);
    let base_def = resolver.resolve_path_in_type_ns_fully(db, path)?;
```
(lines 432-433; the shadowed `ModPath` stays alive to the end of the function.)

> forwarded to db (from requests, pass 16; told by tmux).

### crates/hir/src/display.rs

- line 252-262 (in `write_function`, the async return type):
  ```old
                    *data.store[path]
                        .segments()
                        .iter()
                        .last()
                        .unwrap()
                        .args_and_bindings
                        .unwrap()
                        .bindings[0]
                        .type_ref
                        .as_ref()
                        .unwrap(),
  ```
  ```new
                    data.store
                        .path(path)
                        .segments()
                        .iter()
                        .last()
                        .unwrap()
                        .args_and_bindings
                        .unwrap()
                        .bindings
                        .get(0)
                        .unwrap()
                        .type_ref
                        .unwrap(),
  ```
- line 298: `hir_display_with_store(&impl_data.store[target_trait.path], impl_.into(), &impl_data.store)` -> `hir_display_with_store(impl_data.store.path(target_trait.path), impl_.into(), &impl_data.store)`
  (`T = Path<'a>` borrowing `impl_data.store`, which is `&'a`; needs hir-ide's `impl .. for Path<'_>` above)

- line 834: `lifetimes.iter().map(|it| it.display(f.db, f.edition())).join(", ");` -> `store.names(*lifetimes).iter().map(|it| it.display(f.db, f.edition())).join(", ");`
  (`write_where_predicates` gets the generics' own store at both calls, 148 and 780)

Unchanged: 328-329 `TypeRef::Path(p) if p.is_self_type()` (by-value view), 793, 817-850 otherwise.

> forwarded to db (from requests, pass 16; told by tmux).

### crates/hir/src/source_analyzer.rs

Assumes `ExprCollector::lower_path` returns `Option<PathIdx>` (see "For speed-hir-store-paths").

- lines 961-967 (in `resolve_record_field`):
  ```old
            let path = Path::from_known_path_with_no_generic(ModPath::from_segments(
                PathKind::Plain,
                once(local_name.clone()),
            ));
            match self.resolver.resolve_path_in_value_ns_fully(
                db,
                &path,
  ```
  ```new
            let mod_path = ModPath::from_segments(PathKind::Plain, once(local_name.clone()));
            let path = Path::from_known_path_with_no_generic(&mod_path);
            match self.resolver.resolve_path_in_value_ns_fully(
                db,
                path,
  ```
- lines 1287-1291 (in `resolve_path`):
  ```old
        let hir_path =
            collector.lower_path(path.clone(), &mut ExprCollector::impl_trait_error_allocator)?;
        let parent_hir_path = path
            .parent_path()
            .and_then(|p| collector.lower_path(p, &mut ExprCollector::impl_trait_error_allocator));
        let (store, _) = collector.store.finish();
  ```
  ```new
        let hir_path =
            collector.lower_path(path.clone(), &mut ExprCollector::impl_trait_error_allocator)?;
        let parent_hir_path = path
            .parent_path()
            .and_then(|p| collector.lower_path(p, &mut ExprCollector::impl_trait_error_allocator));
        let (store, _) = collector.store.finish();
        let hir_path = store.path_at(hir_path);
        let parent_hir_path = parent_hir_path.map(|it| store.path_at(it));
  ```
- line 1300: `return resolve_hir_path_qualifier(db, &self.resolver, &hir_path, &store)` -> `return resolve_hir_path_qualifier(db, &self.resolver, hir_path, &store)`
- line 1317: `return match resolve_hir_path_qualifier(db, &self.resolver, &hir_path, &store) {` -> `return match resolve_hir_path_qualifier(db, &self.resolver, hir_path, &store) {`
- line 1405: `return match resolve_hir_path_as_attr_macro(db, &self.resolver, &hir_path) {` -> `return match resolve_hir_path_as_attr_macro(db, &self.resolver, hir_path) {`
- line 1421: `resolve_hir_path_qualifier(db, &self.resolver, &hir_path, &store).map(|it| (it, None))` -> `resolve_hir_path_qualifier(db, &self.resolver, hir_path, &store).map(|it| (it, None))`
- line 1428: `                &hir_path,` -> `                hir_path,`
- lines 1495-1500 (in `resolve_hir_path_per_ns`):
  ```old
        let (store, _) = collector.store.finish();
        Some(resolve_hir_path_(
            db,
            &self.resolver,
            self.infer_body,
            &hir_path,
  ```
  ```new
        let (store, _) = collector.store.finish();
        let hir_path = store.path_at(hir_path);
        Some(resolve_hir_path_(
            db,
            &self.resolver,
            self.infer_body,
            hir_path,
  ```
- lines 1641 and 1681 (both, in `resolve_offset_in_format_args` and `as_format_args_parts`): `&Path::from_known_path_with_no_generic(ModPath::from_segments(` -> `Path::from_known_path_with_no_generic(&ModPath::from_segments(`
  (the `ModPath` temporary lives to the end of the enclosing expression; the result doesn't borrow it)
- line 1855: `    path: &Path,` -> `    path: Path<'_>,` (`resolve_hir_path`)
- line 1866: `    path: &Path,` -> `    path: Path<'_>,` (`resolve_hir_path_as_attr_macro`)
- line 1878: `    path: &Path,` -> `    path: Path<'_>,` (`resolve_hir_path_`)
- line 2046: `    path: &Path,` -> `    path: Path<'_>,` (`resolve_hir_value_path`)
- line 2086: `    path: &Path,` -> `    path: Path<'_>,` (`resolve_hir_path_qualifier`)

Unchanged and checked: 1050/1054 `Expr::Path(path) => path` / `Pat::Path(path) => path` (by-value
views) passed to `resolve_hir_value_path` at 1065; the bodies of the five functions (`path` is
`Copy`, captured by the `types`/`values`/`items`/`macros` closures; `path.segments().first()`,
`path.mod_path()?`, `path.type_anchor()` and the resolver calls take it by value); 1336-1340
`hir_path.mod_path()`, `parent_hir_path.segments().get(1).map(|it| it.name)`; 2183.

> forwarded to db (from requests, pass 16; told by tmux).

### crates/hir/src/semantics.rs

- line 2465: `let is_not_generated = |path: &Path| {` -> `let is_not_generated = |path: Path<'_>| {`
  (2527 `Expr::Path(path) = store.expr(..)` gives `Some(path)` by value; 2537 `is_not_generated(path)` and 2541 `resolve_path_in_value_ns_fully(self.db, path, hygiene)` pass it as is)
- line 2827: `&Path::BarePath(ModPath::from_segments(kind, segments)),` -> `Path::BarePath(&ModPath::from_segments(kind, segments)),`
- line 2895: `fn resolve_path(&mut self, node: ExprOrPatId, path: &Path) {` -> `fn resolve_path(&mut self, node: ExprOrPatId, path: Path<'_>) {`
  (2912 `if let Path::BarePath(path) = path` binds `&ModPath` as before; caller 2942-2944 passes the by-value `Expr::Path(path)`)

Unchanged: 1730 `TypeRef::Path(path) => path` (by-value `type_ref` view) into `resolve_path_in_type_ns_fully`.

> forwarded to db (from requests, pass 16; told by tmux).

### crates/hir/src/diagnostics.rs

- line 1924: `let args = store[*path].generic_args()?.last()?.as_ref()?;` -> `let args = store.path(*path).generic_args()?.last()??;`

Unchanged: 1771-1773 (`TypeBound::Path(path, _) | TypeBound::ForLifetime(_, path)` bind `&PathId`;
`path.type_ref()`), 1840 `args.bindings.first()?.type_ref?`. Every other hit is a diagnostic's
name (`UnresolvedMethodCall`, `GenericArgsProhibited`, ...).

> forwarded to db (from requests, pass 16; told by tmux).

### hir, no edit

- `lib.rs`: every hit is next-solver `GenericArgs`/`GenericArg`, `Mode::MethodCall`,
  `CallableKind::TupleStruct`, or the private `use hir_def::expr_store::path::Path` (241, still valid).
  `hir::WherePredicate` (6572) is a next-solver clause, not hir-def's.
- `tests.rs`: next-solver `GenericArgs`, `.bindings()` on a binder. `term_search/expr.rs`: `ModPath`.
- `source_analyzer.rs:387`: `where_predicate_must_hold(.., predicate)` (hir-ty `traits.rs`, the `lower` part's).

Status: open

> forwarded to db (from requests, pass 16; told by tmux).

## frontend-nameres-core-macros: `Debug` on `MacroSubNs` (2026-10-29)

From `frontend-nameres-core-macros` (report `/tmp/blaze-lanes/v2-frontend-nameres-core-macros.md`).

`crates/hir-def/src/nameres.rs`, above `pub enum MacroSubNs {`: old
`#[derive(Clone, Copy, PartialEq, Eq)]`, new `#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]`.

Why: `collector.rs`'s `#[derive(Debug)] struct ResolvedMacroPath` (the `NameRecords` work) holds
a `MacroSubNs` and does not compile without it. (The facts resolver's `DeclOp::ResolvedMacroPath`
carries `bang: bool` instead, so it does not need this.)

Status: withdrawn by frontend-nameres-core-macros (2026-10-29, at core's request: the facts op
carries `bang: bool`). Whether collector.rs's `derive(Debug)` needs it is db-expand-names' call.

> forwarded to db (from requests, pass 16; told by tmux).
> frontend-hooks (2026-10-29): nothing to apply. Withdrawn, and `nameres.rs:1277` already has
> `#[derive(Debug, Clone, Copy, PartialEq, Eq)]` (db-expand-names added `Debug`).

## dedup: bits: hir-ty-borrowck onto `blaze_ids::bits` (from dedup-bits-db)

**Superseded (2026-10-29, 20:55): don't apply this section.** It puts borrowck's sets in a
global `Vec`, which conflicts with mem-db-checks B2.1 (frame slices; T1 needs zero global
allocations for a clean body; `v2/requests/mem-db-checks-check.md` B7.13). Apply
`## dedup: bits: hir-ty-borrowck on frame words (supersedes the one above)` instead, after B2
and B7.

From `dedup-bits-db` (report `/tmp/blaze-lanes/v2-dedup-bits-db.md`), for lane `dedup-bits`.

Why: `crates/hir-ty-borrowck/src/bitset.rs` is one of a dozen copies of the same `u64`-word bit
set; `blaze_ids::bits` (`crates/blaze-ids/src/bits.rs`) is the one family now. Its `BitSet`,
`BitMatrix`, `masks` (`range_masks`), `union_words`, `iter_words` and `words_for` are deleted; the
`Transfer` trait stays in borrowck (an analysis trait) and moves to `dataflow.rs` beside
`GenKill`, implemented there for `blaze_ids::bits::BitSet`. `bitset.rs`'s four tests are already
in `crates/blaze-ids/src/bits/tests.rs`. Behavior is unchanged: same members, the same panics
(insert and ranges past the capacity), `contains` past it false. `hir-ty-borrowck/Cargo.toml`
already has `blaze-ids.workspace = true`. Edit by content: `dataflow.rs`'s tests are being
edited by another agent (the `SplitMix64` move), so line numbers drift.

Renames at call sites: `BitSet::union` is `union_with`, `BitSet::load(words)` is
`assign_words(words)`, `iter_words(m.row(r))` is `m.iter_row(r)`. `BitMatrix::row`/`row_mut`
now return `BitSlice`/`BitSliceMut` views, not `&[u64]`/`&mut [u64]`: take `.words()` where a
slice is wanted. `BitMatrix::insert` now returns `bool` (whether it was new), so a closure
that ends in it gains braces (`liveness.rs`'s `locals_in` callers).

> forwarded to db (from requests, pass 16; told by tmux).

### 1. `crates/hir-ty-borrowck/src/bitset.rs`: delete the file.

> forwarded to db (from requests, pass 16; told by tmux).

### 2. `crates/hir-ty-borrowck/src/lib.rs`

Before:

```rust
mod bitset;
mod cfg;
```

After:

```rust
mod cfg;
```

> forwarded to db (from requests, pass 16; told by tmux).

### 3. `crates/hir-ty-borrowck/src/dataflow.rs`

Imports and the `Transfer` trait. Before:

```rust
use std::collections::VecDeque;

use hir_ty_mir::mir::BasicBlockId;

use super::{
    bitset::{BitMatrix, BitSet, Transfer, masks, union_words, words_for},
    cfg::{Cfg, index},
};

```

After:

```rust
use std::{collections::VecDeque, ops::Range};

use blaze_ids::bits::{BitMatrix, BitSet, range_masks, words_for};
use hir_ty_mir::mir::BasicBlockId;

use super::cfg::{Cfg, index};

/// What a statement does to a set: adds and removes bits, whatever is in it. Applied to a
/// [`BitSet`] as a walk goes, or composed into a block's gen and kill sets ([`GenKill::row`]),
/// from the one function either way.
pub(super) trait Transfer {
    fn gen_range(&mut self, range: Range<usize>);
    fn kill_range(&mut self, range: Range<usize>);

    #[inline]
    fn gen_(&mut self, bit: usize) {
        self.gen_range(bit..bit + 1);
    }

    #[inline]
    fn kill(&mut self, bit: usize) {
        self.kill_range(bit..bit + 1);
    }
}

impl Transfer for BitSet {
    #[inline]
    fn gen_range(&mut self, range: Range<usize>) {
        self.insert_range(range);
    }

    #[inline]
    fn kill_range(&mut self, range: Range<usize>) {
        self.remove_range(range);
    }
}

```

In `impl Transfer for Row<'_>`, both `masks(range, |word, mask| {` become
`range_masks(range, |word, mask| {` (two places), and `fn gen_range(&mut self, range:
std::ops::Range<usize>)` / `fn kill_range(&mut self, range: std::ops::Range<usize>)` may drop the
`std::ops::` (now imported; leaving it also compiles).

`forward`, before:

```rust
    for (word, from) in entry.row_mut(index(start)).iter_mut().zip(boundary.words()) {
        *word = *from;
    }
    let mut out = vec![0u64; blocks.words];
    let mut along = vec![0u64; blocks.words];
    let mut queue = WorkQueue::new(count, cfg.order.iter().copied());
    while let Some(at) = queue.pop() {
        out.copy_from_slice(entry.row(index(at)));
        blocks.apply(index(at), &mut out);
        for &to in cfg.successors(at) {
            along.copy_from_slice(&out);
            edge(at, to, &mut along);
            if union_words(entry.row_mut(index(to)), &along) {
                queue.push(to);
            }
        }
    }
```

After (`boundary` has the row's words: every caller sizes it to the analysis's domain):

```rust
    entry.row_mut(index(start)).copy_from(boundary);
    let mut out = BitSet::new(blocks.words * 64);
    let mut along = BitSet::new(blocks.words * 64);
    let mut queue = WorkQueue::new(count, cfg.order.iter().copied());
    while let Some(at) = queue.pop() {
        out.copy_from(&entry.row(index(at)));
        blocks.apply(index(at), out.words_mut());
        for &to in cfg.successors(at) {
            along.copy_from(&out);
            edge(at, to, along.words_mut());
            if entry.union_row_with(index(to), &along) {
                queue.push(to);
            }
        }
    }
```

`backward`, before:

```rust
    let mut exit = vec![0u64; blocks.words];
    let mut along = vec![0u64; blocks.words];
    let mut queue = WorkQueue::new(count, cfg.order.iter().rev().copied());
    while let Some(at) = queue.pop() {
        exit.fill(0);
        for &to in cfg.successors(at) {
            along.copy_from_slice(entry.row(index(to)));
            edge(at, to, &mut along);
            union_words(&mut exit, &along);
        }
        blocks.apply(index(at), &mut exit);
        if entry.row(index(at)) != exit.as_slice() {
            entry.row_mut(index(at)).copy_from_slice(&exit);
```

After:

```rust
    let mut exit = BitSet::new(blocks.words * 64);
    let mut along = BitSet::new(blocks.words * 64);
    let mut queue = WorkQueue::new(count, cfg.order.iter().rev().copied());
    while let Some(at) = queue.pop() {
        exit.clear();
        for &to in cfg.successors(at) {
            along.copy_from(&entry.row(index(to)));
            edge(at, to, along.words_mut());
            exit.union_with(&along);
        }
        blocks.apply(index(at), exit.words_mut());
        if entry.row(index(at)).words() != exit.words() {
            entry.row_mut(index(at)).copy_from(&exit);
```

Tests module, before:

```rust
    use super::{GenKill, backward, forward};
    use crate::{
        bitset::{BitSet, Transfer, iter_words},
        cfg::{Cfg, block, index},
    };
```

After (keep whatever other `use` lines the module has, such as `blaze_ids::rng::SplitMix64`):

```rust
    use blaze_ids::bits::{BitMatrix, BitSet};

    use super::{GenKill, Transfer, backward, forward};
    use crate::cfg::{Cfg, block, index};
```

and before:

```rust
    fn rows(entry: &crate::bitset::BitMatrix, blocks: usize) -> Vec<Vec<usize>> {
        (0..blocks).map(|b| iter_words(entry.row(b)).collect()).collect()
    }
```

After:

```rust
    fn rows(entry: &BitMatrix, blocks: usize) -> Vec<Vec<usize>> {
        (0..blocks).map(|b| entry.iter_row(b).collect()).collect()
    }
```

> forwarded to db (from requests, pass 16; told by tmux).

### 4. `crates/hir-ty-borrowck/src/loans.rs`

Before:

```rust
use super::{
    Ctx,
    bitset::{BitMatrix, BitSet, Transfer},
    cfg::{Cfg, Location, index},
    dataflow::{GenKill, forward},
```

After:

```rust
use blaze_ids::bits::{BitMatrix, BitSet};

use super::{
    Ctx,
    cfg::{Cfg, Location, index},
    dataflow::{GenKill, Transfer, forward},
```

No other change (`BitSet::new`, `insert`, `BitMatrix::new` keep their meaning).

> forwarded to db (from requests, pass 16; told by tmux).

### 5. `crates/hir-ty-borrowck/src/moves.rs`

Before:

```rust
use super::{
    Ctx,
    bitset::{BitMatrix, BitSet, Transfer},
    cfg::{Cfg, Location, index},
    dataflow::{GenKill, forward},
```

After:

```rust
use blaze_ids::bits::{BitMatrix, BitSet};

use super::{
    Ctx,
    cfg::{Cfg, Location, index},
    dataflow::{GenKill, Transfer, forward},
```

No other change (`first_in`, `filled`, `kill_range` through `Transfer`, `insert`).

> forwarded to db (from requests, pass 16; told by tmux).

### 6. `crates/hir-ty-borrowck/src/liveness.rs`

Imports, before:

```rust
use super::{
    Ctx,
    bitset::{BitMatrix, BitSet, Transfer, union_words},
    cfg::{Cfg, Location, index},
    dataflow::{GenKill, backward, forward},
```

After:

```rust
use blaze_ids::bits::{BitMatrix, BitSet};

use super::{
    Ctx,
    cfg::{Cfg, Location, index},
    dataflow::{GenKill, Transfer, backward, forward},
```

`live_ranges`, before:

```rust
    let mut live = BitSet::new(locals);
    let mut along = vec![0u64; live.words().len()];
    for &block in &cfg.order {
        live.clear();
        for &successor in cfg.successors(block) {
            along.copy_from_slice(entry.row(index(successor)));
            edge(block, successor, &mut along);
            union_words(live.words_mut(), &along);
        }
```

After:

```rust
    let mut live = BitSet::new(locals);
    let mut along = BitSet::new(locals);
    for &block in &cfg.order {
        live.clear();
        for &successor in cfg.successors(block) {
            along.copy_from(&entry.row(index(successor)));
            edge(block, successor, along.words_mut());
            live.union_with(&along);
        }
```

`maybe_initialized_at_drops`, before:

```rust
        state.load(entry.row(index(block)));
```

After:

```rust
        state.assign_words(entry.row(index(block)).words());
```

and before:

```rust
        locals_in(moves, &state, |local| exit.insert(index(block), local));
        if returns_to[index(block)].is_some() {
            maybe_init_effect(moves, cfg, terminator, &mut state, true);
            locals_in(moves, &state, |local| returned.insert(index(block), local));
        }
```

After:

```rust
        locals_in(moves, &state, |local| {
            exit.insert(index(block), local);
        });
        if returns_to[index(block)].is_some() {
            maybe_init_effect(moves, cfg, terminator, &mut state, true);
            locals_in(moves, &state, |local| {
                returned.insert(index(block), local);
            });
        }
```

`InitAtExit::restrict`, before:

```rust
        for (word, mask) in live.iter_mut().zip(mask) {
```

After:

```rust
        for (word, mask) in live.iter_mut().zip(mask.words()) {
```

(`values[rv].union(..)` in this file is `IntervalSet::union`: unchanged.)

> forwarded to db (from requests, pass 16; told by tmux).

### 7. `crates/hir-ty-borrowck/src/check.rs`

Before:

```rust
use super::{
    Applicability, BorrowckDiagnostic, Ctx, Mode, PartSpan,
    bitset::BitSet,
    cfg::{Cfg, Dominators, Location, index},
```

After:

```rust
use blaze_ids::bits::BitSet;

use super::{
    Applicability, BorrowckDiagnostic, Ctx, Mode, PartSpan,
    cfg::{Cfg, Dominators, Location, index},
```

Before:

```rust
        checker.uninit.load(flow.uninit_entry.row(index(block)));
        checker.ever.load(flow.ever_entry.row(index(block)));
        checker.in_scope.load(borrows_entry.row(index(block)));
```

After:

```rust
        checker.uninit.assign_words(flow.uninit_entry.row(index(block)).words());
        checker.ever.assign_words(flow.ever_entry.row(index(block)).words());
        checker.in_scope.assign_words(borrows_entry.row(index(block)).words());
```

> forwarded to db (from requests, pass 16; told by tmux).

### 8. `crates/hir-ty-borrowck/src/check/explain.rs`

Before:

```rust
use super::Checker;
use crate::{
    BorrowckDiagnostic, PartSpan,
    bitset::BitSet,
    cfg::Location,
```

After:

```rust
use blaze_ids::bits::BitSet;

use super::Checker;
use crate::{
    BorrowckDiagnostic, PartSpan,
    cfg::Location,
```

> forwarded to db (from requests, pass 16; told by tmux).

### 9. `crates/hir-ty-borrowck/src/scan.rs`

Before:

```rust
use crate::{
    bitset::BitSet,
    regions::{Annotation, spells_generics},
};
```

After:

```rust
use blaze_ids::bits::BitSet;

use crate::regions::{Annotation, spells_generics};
```

> forwarded to db (from requests, pass 16; told by tmux).

### 10. `crates/hir-ty-borrowck/src/polonius.rs`

Before:

```rust
use super::{
    Ctx,
    bitset::BitSet,
    cfg::{Cfg, Location, index},
```

After:

```rust
use blaze_ids::bits::BitSet;

use super::{
    Ctx,
    cfg::{Cfg, Location, index},
```

(`polonius.rs` already has `use blaze_ids::radix;`; the two may merge into
`use blaze_ids::{bits::BitSet, radix};`.)

> forwarded to db (from requests, pass 16; told by tmux).

### 11. `crates/hir-ty-borrowck/src/inference.rs`

Before:

```rust
use super::{
    bitset::{BitMatrix, iter_words},
    intervals::IntervalSet,
```

After:

```rust
use blaze_ids::bits::BitMatrix;

use super::{
    intervals::IntervalSet,
```

Before:

```rust
        iter_words(self.universals.row(self.scc[rv] as usize))
```

After:

```rust
        self.universals.iter_row(self.scc[rv] as usize)
```

> forwarded to db (from requests, pass 16; told by tmux).

### 12. `crates/hir-ty-borrowck/src/regions.rs`

Before:

```rust
use super::{
    Ctx,
    bitset::BitMatrix,
    cfg::{Cfg, Location},
```

After:

```rust
use blaze_ids::bits::BitMatrix;

use super::{
    Ctx,
    cfg::{Cfg, Location},
```

No other change (`BitMatrix::default`, `new`, `insert`, `contains`, `union_rows`).

> forwarded to db (from requests, pass 16; told by tmux).

### 13. `crates/hir-ty-borrowck/src/constraints.rs`: the inline `from_closure` words

Before:

```rust
use hir_ty::mir::MirSpan;

use super::{
```

After:

```rust
use blaze_ids::bits::BitSet;
use hir_ty::mir::MirSpan;

use super::{
```

Before:

```rust
    from_closure: Vec<u64>,
```

After:

```rust
    from_closure: BitSet,
```

Before (in `push`):

```rust
        if at.is_multiple_of(64) {
            self.from_closure.push(0);
        }
        if it.from_closure {
            self.from_closure[at / 64] |= 1 << (at % 64);
        }
```

After (a word per 64 constraints whatever the last bit, as before):

```rust
        self.from_closure.set_growing(at, it.from_closure);
```

Before (in `get`):

```rust
            from_closure: self.from_closure[at / 64] & (1 << (at % 64)) != 0,
```

After:

```rust
            from_closure: self.from_closure.contains(at),
```

Status: superseded by `## dedup: bits: hir-ty-borrowck on frame words (supersedes the one above)`

> forwarded to db (from requests, pass 16; told by tmux).

## dedup: bits: hir-ty `infer/columns.rs` sets onto `blaze_ids::bits` (from dedup-bits-db)

From `dedup-bits-db` (report `/tmp/blaze-lanes/v2-dedup-bits-db.md`), for lane `dedup-bits`.

Why: `Presence` is the rank9 set `blaze_ids::bits::Ranked` is; `IdxSet`, `VarSet` and `set_bit`
are the growable `u64`-word set `blaze_ids::bits::BitSet` is. `Presence` stays as a one-field
wrapper because it keeps an invariant `Ranked` does not (its last word is never zero, so a column
has one form and derived `Eq` means "same keys"); `IdxSet` stays as a wrapper for the same
invariant and its `DenseKey` keys (an `Idx<T>` can't implement `BitIndex` here, both foreign).
`VarSet` and `set_bit` are deleted. Behavior is unchanged: the same members, places and
iteration order, `from_parts`/`from_words` refuse what they refused, and the persisted columns
(`present()`, `words()`) are the same words. `Hash` of `IdxSet` is unchanged (a `BitSet` hashes
its word slice, as a `Vec<u64>` does); `Hash` of `Presence` now also hashes `Ranked`'s `len`
(`words.len() * 64`, a function of the words), so in-memory hash values of `IndexColumn`/`Csr`
change, which is harmless: nothing persists a `std::hash::Hash` value, and equality is the same.
`hir-ty/Cargo.toml` already has `blaze-ids.workspace = true`.

> forwarded to db (from requests, pass 16; told by tmux).

### 1. `crates/hir-ty/src/infer/columns.rs`

Imports, before:

```rust
use blaze_ids::radix;
```

After:

```rust
use blaze_ids::{
    bits::{BitSet, Ranked},
    radix,
};
```

`Presence` and `set_bit`, before (from the doc comment of `struct Presence` through the end of
`fn set_bit`):

```rust
/// Which keys of a sparse column have a row, and where it is: a bit per key, and per word of
/// bits how many keys before it have one, so a row's place is one popcount (rank as in
/// Vigna's rank9). That is 1.5 bits a key where a `u32` place per key is 32, and the ranks are
/// made again from the bits, never stored.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
struct Presence {
    /// Its last word is never zero.
    words: Box<[u64]>,
    ranks: Box<[u32]>,
}

impl Presence {
    fn new(words: Vec<u64>) -> Result<Presence, &'static str> {
        if words.last() == Some(&0) {
            return Err("a sparse column whose last word of keys has none");
        }
        let mut ranks = Vec::with_capacity(words.len());
        let mut before = 0u32;
        for &word in &words {
            ranks.push(before);
            before = before.checked_add(word.count_ones()).ok_or("a column of 2^32 rows")?;
        }
        Ok(Presence { words: words.into_boxed_slice(), ranks: ranks.into_boxed_slice() })
    }

    /// How many keys have a row.
    fn count(&self) -> usize {
        match (self.ranks.last(), self.words.last()) {
            (Some(&rank), Some(word)) => rank as usize + word.count_ones() as usize,
            _ => 0,
        }
    }

    /// The place of `index`'s row among the rows, if it has one.
    #[inline]
    fn place(&self, index: usize) -> Option<usize> {
        let (at, bit) = (index / 64, index % 64);
        let word = *self.words.get(at)?;
        if word & (1 << bit) == 0 {
            return None;
        }
        Some(self.ranks[at] as usize + (word & ((1u64 << bit) - 1)).count_ones() as usize)
    }

    /// Each key with a row and the row's place, in key order.
    fn keys(&self) -> impl DoubleEndedIterator<Item = (usize, usize)> + '_ {
        self.words.iter().zip(&self.ranks).enumerate().flat_map(|(at, (&word, &rank))| {
            (0..64usize).filter(move |&bit| word & (1 << bit) != 0).map(move |bit| {
                (at * 64 + bit, rank as usize + (word & ((1u64 << bit) - 1)).count_ones() as usize)
            })
        })
    }
}

/// Sets `index`'s bit in `words`, growing them.
fn set_bit(words: &mut Vec<u64>, index: usize) {
    let at = index / 64;
    if at >= words.len() {
        words.resize(at + 1, 0);
    }
    words[at] |= 1 << (index % 64);
}
```

After:

```rust
/// Which keys of a sparse column have a row, and where it is: a bit per key, ranked, so a
/// row's place is one popcount. Not a bare [`Ranked`]: its last word is never zero, so a column
/// has one form and two that hold the same keys are equal. The ranks are made again from the
/// bits, never stored.
#[derive(Clone, Default, PartialEq, Eq, Hash)]
struct Presence(Ranked);

impl Presence {
    fn new(words: Vec<u64>) -> Result<Presence, &'static str> {
        if words.last() == Some(&0) {
            return Err("a sparse column whose last word of keys has none");
        }
        let len = words.len() * 64;
        Ranked::from_words(words, len).map(Presence).ok_or("a column of 2^32 rows")
    }

    /// Which keys have a row: bit `key % 64` of word `key / 64`.
    fn words(&self) -> &[u64] {
        self.0.words()
    }

    /// How many keys have a row.
    fn count(&self) -> usize {
        self.0.count()
    }

    /// The place of `index`'s row among the rows, if it has one.
    #[inline]
    fn place(&self, index: usize) -> Option<usize> {
        self.0.place(index)
    }

    /// Each key with a row and the row's place, in key order.
    fn keys(&self) -> impl DoubleEndedIterator<Item = (usize, usize)> + '_ {
        self.0.iter_ranked()
    }
}
```

`IndexColumn::from_entries`, before:

```rust
        let (mut words, mut rows) = (Vec::new(), Vec::new());
        for (index, row) in dense(entries).into_iter().enumerate() {
            if let Some(row) = row {
                set_bit(&mut words, index);
                rows.push(row);
            }
        }
        let present = Presence::new(words).expect("a bit is set in every word that is made");
```

After:

```rust
        let (mut words, mut rows) = (BitSet::<usize>::default(), Vec::new());
        for (index, row) in dense(entries).into_iter().enumerate() {
            if let Some(row) = row {
                words.insert_growing(index);
                rows.push(row);
            }
        }
        let present = Presence::new(words.into_words()).expect("a bit is set in every word that is made");
```

`IndexColumn::present` and `Csr::present` (two places), before:

```rust
        &self.present.words
```

After:

```rust
        self.present.words()
```

`CsrBuilder::freeze`, before:

```rust
        let mut words = Vec::new();
        let mut ends = Vec::new();
```

After:

```rust
        let mut words = BitSet::<usize>::default();
        let mut ends = Vec::new();
```

before:

```rust
            set_bit(&mut words, index);
            in_order &= range.start as usize == next;
```

After:

```rust
            words.insert_growing(index);
            in_order &= range.start as usize == next;
```

before:

```rust
        let present = Presence::new(words).expect("a bit is set in every word that is made");
        let ends = ends.into_boxed_slice();
```

After:

```rust
        let present = Presence::new(words.into_words()).expect("a bit is set in every word that is made");
        let ends = ends.into_boxed_slice();
```

`IdxSet`, before (the struct through `fn shrink_to_fit`'s impl block end):

```rust
/// A set of keys as a bitset, its last word never zero.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct IdxSet<K> {
    words: Vec<u64>,
    key: PhantomData<fn() -> K>,
}

impl<K> Default for IdxSet<K> {
    fn default() -> Self {
        IdxSet { words: Vec::new(), key: PhantomData }
    }
}

impl<K: DenseKey> IdxSet<K> {
    /// Adds `key`, returning whether it was not in the set.
    pub(crate) fn insert(&mut self, key: K) -> bool {
        let index = key.to_index();
        let word = index / 64;
        if word >= self.words.len() {
            self.words.resize(word + 1, 0);
        }
        let bit = 1u64 << (index % 64);
        let new = self.words[word] & bit == 0;
        self.words[word] |= bit;
        new
    }

    #[inline]
    pub fn contains(&self, key: &K) -> bool {
        let index = key.to_index();
        self.words.get(index / 64).is_some_and(|word| word & (1 << (index % 64)) != 0)
    }

    pub fn iter(&self) -> impl Iterator<Item = K> + '_ {
        self.words.iter().enumerate().flat_map(|(at, &word)| {
            (0..64).filter(move |bit| word & (1 << bit) != 0).map(move |bit| K::from_index(at * 64 + bit))
        })
    }

    pub fn len(&self) -> usize {
        self.words.iter().map(|word| word.count_ones() as usize).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    pub fn words(&self) -> &[u64] {
        &self.words
    }

    pub(crate) fn from_words(words: Vec<u64>) -> Result<Self, &'static str> {
        match words.last() {
            Some(0) => Err("a set whose last word is zero"),
            _ => Ok(IdxSet { words, key: PhantomData }),
        }
    }

    pub(crate) fn shrink_to_fit(&mut self) {
        self.words.shrink_to_fit();
    }
}
```

After (it only grows, so its last word stays nonzero):

```rust
/// A set of keys as a bitset, its last word never zero, so two that hold the same keys are
/// equal.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct IdxSet<K> {
    words: BitSet,
    key: PhantomData<fn() -> K>,
}

impl<K> Default for IdxSet<K> {
    fn default() -> Self {
        IdxSet { words: BitSet::default(), key: PhantomData }
    }
}

impl<K: DenseKey> IdxSet<K> {
    /// Adds `key`, returning whether it was not in the set.
    pub(crate) fn insert(&mut self, key: K) -> bool {
        self.words.insert_growing(key.to_index())
    }

    #[inline]
    pub fn contains(&self, key: &K) -> bool {
        self.words.contains(key.to_index())
    }

    pub fn iter(&self) -> impl Iterator<Item = K> + '_ {
        self.words.iter().map(K::from_index)
    }

    pub fn len(&self) -> usize {
        self.words.count()
    }

    pub fn is_empty(&self) -> bool {
        self.words.words().is_empty()
    }

    pub fn words(&self) -> &[u64] {
        self.words.words()
    }

    pub(crate) fn from_words(words: Vec<u64>) -> Result<Self, &'static str> {
        match words.last() {
            Some(0) => Err("a set whose last word is zero"),
            _ => Ok(IdxSet { words: BitSet::from_words(words), key: PhantomData }),
        }
    }

    pub(crate) fn shrink_to_fit(&mut self) {
        self.words.shrink_to_fit();
    }
}
```

`VarSet`, delete (from its doc comment through the end of `impl VarSet`):

```rust
/// The type variables a coercion of `!` reached, by their index: rustc's
/// `diverging_type_vars`, which only ever holds type variables.
#[derive(Clone, Default, Debug)]
pub(crate) struct VarSet {
    words: Vec<u64>,
}

impl VarSet {
    pub(crate) fn insert(&mut self, var: u32) {
        ...
    }

    #[inline]
    pub(crate) fn contains(&self, var: u32) -> bool {
        ...
    }

    pub(crate) fn iter(&self) -> impl Iterator<Item = u32> + '_ {
        ...
    }
}
```

Tests, `presence_places_agree_with_counting_the_keys_before`, before:

```rust
            let mut words = Vec::new();
            keys.iter().for_each(|&k| set_bit(&mut words, k as usize));
            let present = Presence::new(words).unwrap();
```

After:

```rust
            let words: BitSet = keys.iter().map(|&k| k as usize).collect();
            let present = Presence::new(words.into_words()).unwrap();
```

Tests, `var_set_lists_what_it_holds`, before:

```rust
        let mut set = VarSet::default();
        for var in [0, 63, 64, 200, 3, 63] {
            set.insert(var);
        }
```

After:

```rust
        let mut set = BitSet::<u32>::default();
        for var in [0, 63, 64, 200, 3, 63] {
            set.insert_growing(var);
        }
```

(the rest of that test is unchanged: `iter()` yields `u32`, `contains(var)` takes a `u32`).

> forwarded to db (from requests, pass 16; told by tmux).

### 2. `crates/hir-ty/src/infer/unify.rs`

Before:

```rust
use smallvec::SmallVec;
use thin_vec::ThinVec;
```

After:

```rust
use blaze_ids::bits::BitSet;
use smallvec::SmallVec;
use thin_vec::ThinVec;
```

Before:

```rust
    infer::columns::VarSet,
```

After: the line deleted (from the `use crate::{ ... }` list).

Before:

```rust
    pub(super) diverging_type_vars: VarSet,
```

After (rustc's `diverging_type_vars`: the type variables a coercion of `!` reached, by index):

```rust
    pub(super) diverging_type_vars: BitSet<u32>,
```

Before:

```rust
            diverging_type_vars: VarSet::default(),
```

After:

```rust
            diverging_type_vars: BitSet::default(),
```

Before (in `set_diverging`):

```rust
            self.diverging_type_vars.insert(vid.as_u32());
```

After:

```rust
            self.diverging_type_vars.insert_growing(vid.as_u32());
```

> forwarded to db (from requests, pass 16; told by tmux).

### 3. `crates/hir-ty/src/infer/fallback.rs`

Before:

```rust
use rustc_hash::FxHashMap;
```

After:

```rust
use blaze_ids::bits::BitSet;
use rustc_hash::FxHashMap;
```

Before:

```rust
    infer::{InferenceContext, columns::VarSet},
```

After:

```rust
    infer::InferenceContext,
```

Before:

```rust
        let mut diverging_roots = VarSet::default();
```

After:

```rust
        let mut diverging_roots = BitSet::<u32>::default();
```

Before:

```rust
                diverging_roots.insert(self.table.infer_ctxt.root_var(vid).as_u32());
```

After:

```rust
                diverging_roots.insert_growing(self.table.infer_ctxt.root_var(vid).as_u32());
```

(`diverging_roots.contains(...)` and `self.table.diverging_type_vars.iter()` are unchanged:
`contains` takes the `u32`, `iter` yields `u32`s.)

Status: open

> forwarded to db (from requests, pass 16; told by tmux).

## dedup: bits: hir-ty `TargetFeatureSet` onto `blaze_ids::bits::BitArray` (from dedup-bits-db)

From `dedup-bits-db` (report `/tmp/blaze-lanes/v2-dedup-bits-db.md`), for lane `dedup-bits`.

Why: `TargetFeatureSet` is an inline `[u64; WORDS]` set with its own copy of the word ops;
`blaze_ids::bits::BitArray` is that set, with `const` `with_bit`/`has_bit`/`or` for the
`IMPLIED` table built at compile time. It stays as a one-field struct (its `Debug` names the
features, and its `const fn` API takes `TargetFeature`s, used by `hir-ty-codegen`), its own bit
logic deleted; `TargetFeature` becomes a `BitIndex`. Nothing changes for callers: the same API,
72 bytes (asserted), the same `Eq`, and the same `Hash` value (an array and a `Bits` both hash
the word slice, length first). `hir-ty/Cargo.toml` already has `blaze-ids.workspace = true`.

> forwarded to db (from requests, pass 16; told by tmux).

### `crates/hir-ty/src/target_feature.rs`

Imports, before:

```rust
use base_db::{Db, Crate};
```

After:

```rust
use base_db::{Db, Crate};
use blaze_ids::bits::{BitArray, BitIndex};
```

Before (`impl fmt::Debug for TargetFeature` through the end of `impl TargetFeatureSet`):

```rust
/// A set of target features, one bit each: 72 bytes, a test is one load and a shift.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TargetFeatureSet([u64; WORDS]);

const _: () = assert!(size_of::<TargetFeatureSet>() == 72);

impl TargetFeatureSet {
    pub const EMPTY: Self = Self([0; WORDS]);
```

After:

```rust
impl BitIndex for TargetFeature {
    #[inline]
    fn to_bit(self) -> usize {
        self.index()
    }

    #[inline]
    fn from_bit(bit: usize) -> Self {
        Self(<u16 as BitIndex>::from_bit(bit))
    }
}

/// A set of target features, one bit each: 72 bytes, a test is one load and a shift. A struct
/// of its own for a `Debug` that names the features.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct TargetFeatureSet(BitArray<WORDS, TargetFeature>);

const _: () = assert!(size_of::<TargetFeatureSet>() == 72);

impl TargetFeatureSet {
    pub const EMPTY: Self = Self(BitArray::<WORDS, TargetFeature>::EMPTY);
```

Before (`with` through `union`):

```rust
    #[inline]
    pub const fn with(mut self, feature: TargetFeature) -> Self {
        self.0[feature.index() / 64] |= 1 << (feature.index() % 64);
        self
    }

    #[inline]
    pub fn insert(&mut self, feature: TargetFeature) {
        self.0[feature.index() / 64] |= 1 << (feature.index() % 64);
    }

    #[inline]
    pub const fn contains(&self, feature: TargetFeature) -> bool {
        (self.0[feature.index() / 64] >> (feature.index() % 64)) & 1 != 0
    }

    #[inline]
    pub const fn union(&self, other: &Self) -> Self {
        let mut out = *self;
        let mut i = 0;
        while i < WORDS {
            out.0[i] |= other.0[i];
            i += 1;
        }
        out
    }

    /// Whether every feature of `other` is in `self`.
    #[inline]
    pub fn is_superset(&self, other: &Self) -> bool {
        self.0.iter().zip(&other.0).all(|(mine, theirs)| theirs & !mine == 0)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.iter().all(|&word| word == 0)
    }
```

After:

```rust
    #[inline]
    pub const fn with(self, feature: TargetFeature) -> Self {
        Self(self.0.with_bit(feature.index()))
    }

    #[inline]
    pub fn insert(&mut self, feature: TargetFeature) {
        self.0.insert(feature);
    }

    #[inline]
    pub const fn contains(&self, feature: TargetFeature) -> bool {
        self.0.has_bit(feature.index())
    }

    #[inline]
    pub const fn union(&self, other: &Self) -> Self {
        Self(self.0.or(&other.0))
    }

    /// Whether every feature of `other` is in `self`.
    #[inline]
    pub fn is_superset(&self, other: &Self) -> bool {
        other.0.is_subset(&self.0)
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
```

Before (`iter`):

```rust
    pub fn iter(&self) -> impl Iterator<Item = TargetFeature> + '_ {
        self.0.iter().enumerate().flat_map(|(at, &word)| {
            let mut rest = word;
            std::iter::from_fn(move || {
                if rest == 0 {
                    return None;
                }
                let bit = rest.trailing_zeros();
                rest &= rest - 1;
                Some(TargetFeature((at * 64) as u16 + bit as u16))
            })
        })
    }
```

After:

```rust
    pub fn iter(&self) -> impl Iterator<Item = TargetFeature> + '_ {
        self.0.iter()
    }
```

Before (`same`):

```rust
    const fn same(&self, other: &Self) -> bool {
        let mut i = 0;
        while i < WORDS {
            if self.0[i] != other.0[i] {
                return false;
            }
            i += 1;
        }
        true
    }
```

After (`==` is not `const`):

```rust
    const fn same(&self, other: &Self) -> bool {
        let (ours, theirs) = (self.0.array(), other.0.array());
        let mut i = 0;
        while i < WORDS {
            if ours[i] != theirs[i] {
                return false;
            }
            i += 1;
        }
        true
    }
```

`of`, `implied_in`, `names`, `FromIterator`, `Debug`, `IMPLIED` and `target_feature/tests.rs`
are unchanged.

Status: open

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-apply-ty (21:25): `crates/hir-ty/src/target_feature.rs` as written.

## dedup: bits: hir-ty `ParamsInRepr` onto `blaze_ids::bits` (from dedup-bits-db)

From `dedup-bits-db` (report `/tmp/blaze-lanes/v2-dedup-bits-db.md`), for lane `dedup-bits`.

Why: `ParamsInRepr` is a growable bit set (a first word inline, the rest in a `ThinVec`) with its
own bit logic. As `blaze_ids::bits::Bits<SmallVec<[u64; 1]>>` it is 24 bytes instead of 16 and
still allocates nothing for an ADT of up to 64 parameters (nearly all). 8 bytes more per ADT's
memo is small next to the memo's own revision data, so the 16-byte layout is not worth a copy of
the set kept on purpose. The same members; `contains` past the words is false, as `get` was.
`ParamsInRepr` is private to the file, so nothing outside changes. `hir-ty/Cargo.toml` already
has `blaze-ids.workspace = true` and `smallvec.workspace = true`.

> forwarded to db (from requests, pass 16; told by tmux).

### `crates/hir-ty/src/representability.rs`

Before:

```rust
use base_db::Db;
use hir_def::{AdtId, VariantId};
use rustc_type_ir::inherent::IntoKind;
use thin_vec::ThinVec;
```

After:

```rust
use base_db::Db;
use blaze_ids::bits::Bits;
use hir_def::{AdtId, VariantId};
use rustc_type_ir::inherent::IntoKind;
use smallvec::SmallVec;
```

Before (the doc comment through the end of `impl ParamsInRepr`):

```rust
/// Which of an ADT's generic parameters, by index, appear in its representation: a bitset, the
/// first 64 parameters in one word, which allocates nothing for nearly every ADT.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
struct ParamsInRepr {
    first: u64,
    rest: ThinVec<u64>,
}

const _: () = assert!(size_of::<ParamsInRepr>() == 16);

impl ParamsInRepr {
    #[inline]
    fn get(&self, index: usize) -> bool {
        ...
    }

    fn insert(&mut self, index: usize) {
        ...
    }
}
```

After:

```rust
/// Which of an ADT's generic parameters, by index, appear in its representation: a bitset, the
/// first 64 parameters in its one inline word, which allocates nothing for nearly every ADT.
type ParamsInRepr = Bits<SmallVec<[u64; 1]>>;

const _: () = assert!(size_of::<ParamsInRepr>() == 24);
```

In `representability_adt_ty`, before:

```rust
            && params_in_repr.get(i)
```

After:

```rust
            && params_in_repr.contains(i)
```

In `params_in_repr`, before:

```rust
    params_in_repr.rest.shrink_to_fit();
    params_in_repr
```

After:

```rust
    params_in_repr.shrink_to_fit();
    params_in_repr
```

In `params_in_repr_ty`, before:

```rust
                    && inner_params_in_repr.get(i)
```

After:

```rust
                    && inner_params_in_repr.contains(i)
```

before:

```rust
        TyKind::Param(param) => params_in_repr.insert(param.index as usize),
```

After:

```rust
        TyKind::Param(param) => {
            params_in_repr.insert_growing(param.index as usize);
        }
```

Tests, before:

```rust
    #[test]
    fn params_in_repr_is_a_bitset_past_one_word() {
        let mut set = ParamsInRepr::default();
        for index in [0, 5, 63, 64, 130] {
            set.insert(index);
        }
        let set_bits: Vec<usize> = (0..200).filter(|&it| set.get(it)).collect();
        assert_eq!(set_bits, [0, 5, 63, 64, 130]);
        assert_eq!(set.rest.len(), 2);
    }

    #[test]
    fn params_in_repr_of_few_params_allocates_nothing() {
        let mut set = ParamsInRepr::default();
        set.insert(3);
        set.insert(40);
        assert!(set.rest.is_empty());
        assert!(set.get(3) && set.get(40) && !set.get(4) && !set.get(64));
    }
```

After:

```rust
    #[test]
    fn params_in_repr_is_a_bitset_past_one_word() {
        let mut set = ParamsInRepr::default();
        for index in [0, 5, 63, 64, 130] {
            set.insert_growing(index);
        }
        let set_bits: Vec<usize> = (0..200).filter(|&it| set.contains(it)).collect();
        assert_eq!(set_bits, [0, 5, 63, 64, 130]);
        assert_eq!(set.words().len(), 3);
    }

    #[test]
    fn params_in_repr_of_few_params_allocates_nothing() {
        let mut set = ParamsInRepr::default();
        set.insert_growing(3);
        set.insert_growing(40);
        assert!(!set.into_words().spilled());
        let mut set = ParamsInRepr::default();
        set.insert_growing(3);
        set.insert_growing(40);
        assert!(set.contains(3) && set.contains(40) && !set.contains(4) && !set.contains(64));
    }
```

(`thin_vec` stays a dependency of `hir-ty` for its other users.)

Status: open

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-apply-ty (21:25), on top of mem-db-infer-method R1 (db-mem-infer-b had already
> moved the walk into a frame `ParamsInReprBuilder`, so the "before" texts no longer matched):
> `type ParamsInRepr = Bits<SmallVec<[u64; 1]>>` (24 bytes asserted); the builder keeps its
> frame as `words: Bits<Vec<u64, &'f Frame>>` with `insert_growing`, and `finish` is
> `Bits::from_words(SmallVec::from_slice(..))` (inline up to 64 parameters, one exact allocation
> past); `get` -> `contains`; the two tests check `words().len() == 3` and `!spilled()`.
> No `shrink_to_fit` needed: `from_slice` sizes it exactly. `ThinVec` import gone.

## dedup: TerminatorKind::all_successors (from dedup-scan)

Four copies of "every successor, the unwind last" (borrowck `cfg.rs` `successors`, hir-ty-mir
`optimize.rs` `successors`, blaze-items `form.rs` `push_successors` reversed; all agree on every
terminator). In `crates/hir-ty/src/mir.rs`, after `TerminatorKind::unwind`:
```rust
/// Every block this terminator may go to: its successors, the imaginary edge of a
/// `FalseEdge`, and the block it unwinds to, last.
pub fn all_successors(&self) -> SmallVec<[BasicBlockId; 2]> {
    match self {
        TerminatorKind::FalseEdge { real_target, imaginary_target } => {
            smallvec![*real_target, *imaginary_target]
        }
        _ => {
            let mut all = self.successors();
            all.extend(self.unwind());
            all
        }
    }
}
```
`crates/hir-ty-borrowck/src/cfg.rs`: delete `pub(super) fn successors(kind: &TerminatorKind)` and
call `kind.all_successors()` where it was called (`Cfg::successors(&self, block)` keeps its name).
Callers in codegen.md and items.md.

> forwarded to db (from requests, pass 16; told by tmux).
> applied by db-apply-ty (21:25): `TerminatorKind::all_successors` after `unwind` in `crates/hir-ty/src/mir.rs` as written; `cfg.rs` loses `successors`, `Cfg::new` calls `terminator.kind.all_successors()`, and its now unused `TerminatorKind` import goes.

## dedup: ARITH_OPS is ArithOp::ALL (from dedup-scan)

`crates/hir-def/src/hir/columns.rs` and `crates/hir-ty/src/infer/persist.rs` each have
`const ARITH_OPS: [ArithOp; 10]` in `syntax::ast::ArithOp`'s declaration order; both are
persisted codes. After frontend-tree adds `ArithOp::ALL` (frontend-tree.md), delete both consts
and use `ArithOp::ALL` at each `ARITH_OPS` (columns.rs 571, 578, 2022, 2023; persist.rs 424, 431,
2349, 2943). Same order, so every code decodes as before.

> forwarded to db (from requests, pass 16; told by tmux).

## dedup: block_may_start_with_inner_attr once, in syntax::view (from dedup-scan)

`crates/hir-def/src/attrs/docs.rs`'s `block_may_start_with_inner_doc` and `block_comment_len` are
syntax's `block_may_start_with_inner_attr` and `block_comment_len` (in both `ast/traits.rs` and
`view/traits.rs`) except that `//!` and `/*!` answer true. After frontend-tree makes the view one
`pub fn block_may_start_with_inner_attr(block: &str, inner_docs: bool) -> bool`
(frontend-tree.md), delete both functions from docs.rs and call
`syntax::view::block_may_start_with_inner_attr(block, true)` where
`block_may_start_with_inner_doc(block)` was.

> forwarded to db (from requests, pass 16; told by tmux).

> applied by db-apply-def (21:10): both functions deleted from `hir-def/src/attrs/docs.rs`; `inner_docs_node` and the test `a_block_may_start_with_inner_docs` call `syntax::view::block_may_start_with_inner_attr(.., true)`.

## dedup: bits: hir-ty `mir/held.rs` off `rustc_index::DenseBitSet` onto `blaze_ids::bits::BitSet` (from dedup-bits-db)

From `dedup-bits-db` (report `/tmp/blaze-lanes/v2-dedup-bits-db.md`), for lane `dedup-bits`.

Why: `held.rs`'s `Set` is a local analysis set, not a type of a rustc interface, so it is the one
bit set `blaze_ids::bits::BitSet` (the `DenseBitSet`s that stay in hir-ty are
`UnsizingParams`, rustc_type_ir's `Interner::UnsizingParams` target, and `layout.rs`'s
`BitMatrix`, the argument of rustc_abi's `layout_of_coroutine`). Same members and results:
every set of one analysis is made with the same size, so equality (`live_in[..] != state`) is
unchanged. One check is weaker: `DenseBitSet::insert` asserts the bit is below the domain size,
`BitSet::insert` only below the word capacity (the next multiple of 64); every index here is a
local of the body, below `count`. `hir-ty/Cargo.toml` already has `blaze-ids.workspace = true`.

> forwarded to db (from requests, pass 16; told by tmux).

### `crates/hir-ty/src/mir/held.rs`

Before:

```rust
use rustc_index::bit_set::DenseBitSet;
use smallvec::SmallVec;
```

After:

```rust
use blaze_ids::bits::BitSet;
use smallvec::SmallVec;
```

Before:

```rust
type Set = DenseBitSet<usize>;
```

After:

```rust
type Set = BitSet;
```

Renames, every occurrence in the file (tests included):

- `Set::new_empty(` becomes `Set::new(` (18 places);
- `Set::new_filled(` becomes `Set::filled(` (1 place, `always_storage_live`);
- `.union(` becomes `.union_with(` (8 places: lines about 231, 244, 362, 425, 432, 438, 541,
  653; every one is on a `Set`, and line 362's `changed |= known.union_with(new)` keeps its
  `bool`);
- `.intersect(` becomes `.intersect_with(` (4 places: about 232, 534, 647, 651).

In the tests' `model`, before:

```rust
        let count = held.domain_size();
```

After (`together` is indexed by locals, all below it):

```rust
        let count = held.capacity();
```

`insert`, `remove`, `contains`, `clear`, `clone_from`, `clone`, `iter` and `!=` keep their
meaning.

Status: open

> forwarded to db (from requests, pass 16; told by tmux).
> not applied by db-mem-infer-b-eval (21:30): overtaken. mem-db-infer-eval.md H1 (applied) makes `held.rs`'s `Set` a frame-slice set of its own (`Set<'f>(&'f mut [u64])`), so the file no longer uses `DenseBitSet` (only its tests, as the reference model); a `BitSet` would put each set's words back on the global heap.

## frontend-db: the `compile_fail` test that `HirDatabase` is not dyn-compatible doesn't run (2026-10-29)

`frontend-db-ty` sealed `HirDatabase` and gave it `const NOT_DYN: ()`, so `dyn HirDatabase` is a
compile error, and wrote the `compile_fail` doc test that says so (db.md's request, section 5).
`crates/hir-ty/Cargo.toml:13` has `doctest = false` (hir, hir-ide and base-db too), so the test
never runs. The compiler enforces the property either way (E0038 on any `dyn HirDatabase`); the
test only documents it. Choose one:
- `doctest = false` becomes `doctest = true` in `crates/hir-ty/Cargo.toml`. Every other doc
  example in hir-ty then runs too, unless it is marked `ignore`/`text`.
- or the test goes and its line in the notes' proof list becomes "enforced by `NOT_DYN`",
  which frontend-db will write.
Answer with a `>` line.

Status: open

> forwarded to db (from requests, pass 16; told by tmux).
> frontend-hooks (2026-10-29): not applied, a choice for db (owner of `crates/hir-ty/`,
> db-ty-next2), not an exact edit. Left for db.

> db (db-next3, 21:15): the second. The test goes, and the proof list says "enforced by
> `NOT_DYN`" (frontend-db writes that line). hir-ty keeps `doctest = false`: its doc examples
> are prose, not tests. db-apply-ty deletes the `compile_fail` block.
> applied by db-apply-ty (21:20): the `compile_fail` block above `pub trait HirDatabase` in
> `crates/hir-ty/src/db.rs` is gone (its lead-in sentence now ends in a full stop); `NOT_DYN` and
> `doctest = false` stay.

## dedup: blaze_query::query_kinds! (from dedup-scan)

`crates/hir-def/src/nameres/facts/kinds.rs` and `crates/hir-ty/src/db.rs`'s `pub mod kinds` each
define the same `macro_rules! kinds`, `const NAMES` table and `fn name` (only the range and the
word in the assert differ). One macro in `crates/blaze-query/src/lib.rs`, next to `QueryKind`:
```rust
/// The kind numbers of one crate's queries: `RANGE`, a constant for each kind, [`ALL`] with
/// each query's name, `of(name)`, and `name(kind)` from a table built when compiled, which
/// refuses a number outside `RANGE` or given twice.
#[macro_export]
macro_rules! query_kinds {
    (
        range: $range:expr, owner: $owner:literal;
        $($(#[$meta:meta])* $name:ident = $number:literal, $query:literal;)*
    ) => {
        pub use $crate::QueryKind;

        #[doc = ::core::concat!("The numbers ", $owner, " may give.")]
        pub const RANGE: ::core::ops::Range<u16> = $range;

        $($(#[$meta])* pub const $name: QueryKind = QueryKind($number);)*

        /// Every kind, with its query as its crate names it: a function's name, or
        /// `Type::method` for a method of a tracked `impl`.
        pub const ALL: &[(QueryKind, &str)] = &[$(($name, $query)),*];

        /// The kind of the query named `name`, as [`ALL`] names it.
        pub fn of(name: &str) -> Option<QueryKind> {
            match name {
                $($query => Some($name),)*
                _ => None,
            }
        }

        const NAMES: [Option<&str>; (RANGE.end - RANGE.start) as usize] = {
            let mut names = [None; (RANGE.end - RANGE.start) as usize];
            let mut at = 0;
            while at < ALL.len() {
                let (kind, name) = ALL[at];
                ::core::assert!(
                    RANGE.start <= kind.0 && kind.0 < RANGE.end,
                    ::core::concat!("a kind outside ", $owner, "'s range"),
                );
                let slot = (kind.0 - RANGE.start) as usize;
                ::core::assert!(names[slot].is_none(), "a kind given to two queries");
                names[slot] = Some(name);
                at += 1;
            }
            names
        };

        /// The query of `kind`, if it is this crate's: one bounds check and one load.
        pub fn name(kind: QueryKind) -> Option<&'static str> {
            NAMES.get(kind.0.wrapping_sub(RANGE.start) as usize).copied().flatten()
        }
    };
}
```
- `hir-def/src/nameres/facts/kinds.rs`: keep the module doc and tests; replace `pub use`,
  `RANGE`, the macro, `NAMES` and `name` with
  `blaze_query::query_kinds! { range: 0x2200..0x2240, owner: "the resolver"; MODULE_DECLS = 0x2200, "module_decls"; … }`
  (the current `kinds! { … }` body unchanged).
- `hir-ty/src/db.rs` `pub mod kinds`: the same, `range: 0x2400..0x2600, owner: "hir-ty";`.

Numbers and names are unchanged, so traces and the explainer read as before.
`hir-def/src/expr_store/persist.rs`'s `kinds!` is another macro (column enums) and stays.

> forwarded to db (from requests, pass 16; told by tmux).

> applied in part by db (db-next3, 21:05): `blaze_query::query_kinds!` is in `crates/blaze-query/src/lib.rs` as written, with a test in `tests.rs`. The two users (`hir-def/src/nameres/facts/kinds.rs`, `hir-ty/src/db.rs` `pub mod kinds`) go to db-apply-def and db-apply-ty.
> applied by db-apply-ty (21:20): `hir-ty/src/db.rs` `pub mod kinds` is `blaze_query::query_kinds! { range: 0x2400..0x2600, owner: "hir-ty"; … }` with the kind lines unchanged; its tests stay (`use super::*` gets `QueryKind`, `RANGE`, `ALL`, `of`, `name` from the macro).

## dedup: bits: hir-def `item_scope.rs` `Bits`, `signatures.rs` `VariantBits`, `import_map.rs` `trait_assoc_items` onto `blaze_ids::bits` (from dedup-bits-db)

From `dedup-bits-db` (report `/tmp/blaze-lanes/v2-dedup-bits-db.md`), for lane `dedup-bits`.

Why: three more copies of the `u64`-word bit set, each with its own bit logic, onto the one
`blaze_ids::bits::Bits` over the same storage: `item_scope.rs`'s `Bits` is
`Bits<ThinVec<u64>>` (`set_growing` gives a set kept beside a list the list's words whatever its
last bit, as `push` did), `VariantBits` is `Bits<SmallVec<[u64; 1]>>` (still 24 bytes, asserted,
inline up to 64 variants), and `ImportMap::trait_assoc_items` is a `BitSet`. Same members, same
word counts, so the same `Eq` (`ItemScope`'s and `EnumVariants`' hand-written `eq` compare these
fields). Two reads no longer panic on an index past the words (`VariantBits::get` indexed;
`contains` is false there); every index is a variant or row of the list beside it, so none is.
The derived `Debug` now lists the members instead of the words. `hir-def/Cargo.toml` already has
`blaze-ids.workspace = true`.

> forwarded to db (from requests, pass 16; told by tmux).

### 1. `crates/hir-def/src/item_scope.rs`

Before:

```rust
use blaze_ids::radix;
```

After:

```rust
use blaze_ids::{bits::Bits, radix};
```

Before:

```rust
/// A bit per position of a list beside it, 64 to a word; the bits past the list's end are clear,
/// so two of the same length are equal when their words are.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Bits(ThinVec<u64>);

impl Bits {
    /// Sets bit `at` to `bit`, for the position the list just gave its newest element.
    pub(crate) fn push(&mut self, at: usize, bit: bool) {
        if at % 64 == 0 {
            self.0.push(0);
        }
        self.0[at / 64] |= u64::from(bit) << (at % 64);
    }

    pub(crate) fn get(&self, at: usize) -> bool {
        self.0.get(at / 64).is_some_and(|word| word >> (at % 64) & 1 == 1)
    }
}

```

After:

```rust
/// A bit per position of a list beside it, set by `set_growing` as the list grows, so it has
/// the list's words and two of the same length are equal when their words are.
pub(crate) type ListBits = Bits<ThinVec<u64>>;

```

Fields, before:

```rust
    pub(crate) trait_impl: Bits,
    /// A bit per impl of `impls`, by position: whether it is `#[rustc_reservation_impl]`.
    pub(crate) reservation_impl: Bits,
```

After:

```rust
    pub(crate) trait_impl: ListBits,
    /// A bit per impl of `impls`, by position: whether it is `#[rustc_reservation_impl]`.
    pub(crate) reservation_impl: ListBits,
```

Before:

```rust
    fn impls_where(&self, bits: &Bits, set: bool) -> impl Iterator<Item = ImplId> + '_ {
        self.impls
            .iter()
            .enumerate()
            .filter(move |&(at, _)| bits.get(at) == set)
```

After:

```rust
    fn impls_where(&self, bits: &ListBits, set: bool) -> impl Iterator<Item = ImplId> + '_ {
        self.impls
            .iter()
            .enumerate()
            .filter(move |&(at, _)| bits.contains(at) == set)
```

Before:

```rust
            .filter(|&(at, _)| self.trait_impl.get(at) && !self.reservation_impl.get(at))
```

After:

```rust
            .filter(|&(at, _)| self.trait_impl.contains(at) && !self.reservation_impl.contains(at))
```

Before (`define_impl`):

```rust
        self.trait_impl.push(at, is_trait_impl);
        self.reservation_impl.push(at, is_reservation);
```

After:

```rust
        self.trait_impl.set_growing(at, is_trait_impl);
        self.reservation_impl.set_growing(at, is_reservation);
```

Before (the `shrink_to_fit` list):

```rust
        trait_impl.0.shrink_to_fit();
        reservation_impl.0.shrink_to_fit();
```

After:

```rust
        trait_impl.shrink_to_fit();
        reservation_impl.shrink_to_fit();
```

> forwarded to db (from requests, pass 16; told by tmux).

### 2. `crates/hir-def/src/nameres/frozen.rs`

Before:

```rust
            self.bool(trait_impl.get(at));
            self.bool(reservation_impl.get(at));
```

After:

```rust
            self.bool(trait_impl.contains(at));
            self.bool(reservation_impl.contains(at));
```

Before:

```rust
            scope.trait_impl.push(at, self.bool()?);
            scope.reservation_impl.push(at, self.bool()?);
```

After (the persisted bytes are unchanged: a bool a position):

```rust
            scope.trait_impl.set_growing(at, self.bool()?);
            scope.reservation_impl.set_growing(at, self.bool()?);
```

> forwarded to db (from requests, pass 16; told by tmux).

### 3. `crates/hir-def/src/nameres/facts/tests.rs`

Before:

```rust
                    .map(|(at, &it)| (self.impl_(it), trait_impl.get(at), reservation_impl.get(at)))
```

After:

```rust
                    .map(|(at, &it)| (self.impl_(it), trait_impl.contains(at), reservation_impl.contains(at)))
```

> forwarded to db (from requests, pass 16; told by tmux).

### 4. `crates/hir-def/src/signatures.rs`

Imports, before:

```rust
use smallvec::{SmallVec, smallvec};
```

After (`smallvec!` had no other use in the file):

```rust
use blaze_ids::bits::Bits;
use smallvec::SmallVec;
```

Before:

```rust
/// One bit per variant, by index, kept inline up to 64 variants: no allocation for almost
/// every enum. Bits past the last variant are zero, so equal sets are equal words.
#[derive(Debug, Default, PartialEq, Eq)]
struct VariantBits(SmallVec<[u64; 1]>);

// The inline word and its length, or a heap pointer and its length, plus the capacity.
const _: () = assert!(size_of::<VariantBits>() == 24);

impl VariantBits {
    /// `len` bits, all clear.
    fn with_len(len: usize) -> VariantBits {
        VariantBits(smallvec![0; len.div_ceil(64)]) // alloc: inline up to 64 variants
    }

    fn get(&self, index: usize) -> bool {
        (self.0[index / 64] >> (index % 64)) & 1 == 1
    }

    fn set(&mut self, index: usize, value: bool) {
        let (word, bit) = (&mut self.0[index / 64], index % 64);
        *word = (*word & !(1 << bit)) | (u64::from(value) << bit);
    }

    /// Keeps the words of the first `len` bits, the rest being clear.
    fn truncate(&mut self, len: usize) {
        self.0.truncate(len.div_ceil(64));
        self.0.shrink_to_fit();
    }
}
```

After:

```rust
/// One bit per variant, by index, kept inline up to 64 variants: no allocation for almost
/// every enum. Bits past the last variant are zero, so equal sets are equal words.
type VariantBits = Bits<SmallVec<[u64; 1]>>;

// The inline word and its length, or a heap pointer and its length, plus the capacity.
const _: () = assert!(size_of::<VariantBits>() == 24);
```

Before:

```rust
        let mut explicit_discriminant = VariantBits::with_len(count);
```

After:

```rust
        let mut explicit_discriminant = VariantBits::new(count); // alloc: inline up to 64 variants
```

Before:

```rust
            explicit_discriminant.truncate(variants.len());
```

After:

```rust
            explicit_discriminant.truncate(variants.len());
            explicit_discriminant.shrink_to_fit();
```

`explicit_discriminant.set(index, explicit)` is unchanged. In `has_explicit_discriminant` and
`with_explicit_discriminant`, before:

```rust
            .is_some_and(|index| self.explicit_discriminant.get(index))
```

```rust
            .map(|(index, &(id, shape))| (id, shape, self.explicit_discriminant.get(index)))
```

After:

```rust
            .is_some_and(|index| self.explicit_discriminant.contains(index))
```

```rust
            .map(|(index, &(id, shape))| (id, shape, self.explicit_discriminant.contains(index)))
```

Test `variant_bits_as_bools`: `VariantBits::with_len(` becomes `VariantBits::new(` (four
places), `bits.get(index)` becomes `bits.contains(index)` (three places), `bits.set(...)` and
`bits.truncate(kept)` are unchanged, and the last two lines, before:

```rust
        assert!(!VariantBits::with_len(64).0.spilled());
        assert!(VariantBits::with_len(65).0.spilled());
```

After:

```rust
        assert!(!VariantBits::new(64).into_words().spilled());
        assert!(VariantBits::new(65).into_words().spilled());
```

> forwarded to db (from requests, pass 16; told by tmux).

### 5. `crates/hir-def/src/import_map.rs`

Imports, before:

```rust
use base_db::{Db, Crate};
use fst::{Automaton, Streamer, raw::IndexedValue};
```

After:

```rust
use base_db::{Db, Crate};
use blaze_ids::bits::BitSet;
use fst::{Automaton, Streamer, raw::IndexedValue};
```

Field, before:

```rust
    /// Bit `i` set: row `i` is an associated item of a trait.
    trait_assoc_items: Box<[u64]>,
```

After:

```rust
    /// Bit `i` set: row `i` is an associated item of a trait.
    trait_assoc_items: BitSet,
```

Before:

```rust
        let mut trait_assoc_items = vec![0u64; rows.div_ceil(64)];
        for (row, &is_trait_assoc_item) in found.trait_assoc_items.iter().enumerate() {
            if is_trait_assoc_item {
                trait_assoc_items[row / 64] |= 1 << (row % 64);
            }
        }
```

After:

```rust
        let mut trait_assoc_items = BitSet::new(rows);
        for (row, &is_trait_assoc_item) in found.trait_assoc_items.iter().enumerate() {
            if is_trait_assoc_item {
                trait_assoc_items.insert(row);
            }
        }
```

Before:

```rust
            trait_assoc_items: trait_assoc_items.into_boxed_slice(),
```

After:

```rust
            trait_assoc_items,
```

Before:

```rust
        if self.trait_assoc_items[row / 64] & (1 << (row % 64)) != 0 {
```

After:

```rust
        if self.trait_assoc_items.contains(row) {
```

Status: open

> forwarded to db (from requests, pass 16; told by tmux).

## dedup: bits: blaze-query `Marks` onto `blaze_ids::bits::BitSet` (from dedup-bits-db)

From `dedup-bits-db` (report `/tmp/blaze-lanes/v2-dedup-bits-db.md`), for lane `dedup-bits`.

Why: `Marks` is a growable `u64`-word set of memo IDs with its own bit logic; it is
`blaze_ids::bits::BitSet<MemoId>` (`MemoId` is `u32`, a `BitIndex`), sized by `new` and grown
by `insert_growing` as `Marks::insert` grew. Same members, same "whether it was new" result.
`blaze-query/Cargo.toml` already has `blaze-ids.workspace = true`.

> forwarded to db (from requests, pass 16; told by tmux).

### `crates/blaze-query/src/lib.rs`

Imports, before:

```rust
use blaze_ids::{ContentId, Fingerprint, Hasher, Idx};
```

After:

```rust
use blaze_ids::{ContentId, Fingerprint, Hasher, Idx, bits::BitSet};
```

Delete `struct Marks` and its `impl` (from the doc comment "A set of memo IDs as a bit each" through
the end of `fn iter`'s impl block):

```rust
/// A set of memo IDs as a bit each, sized by the IDs handed out: marking or testing one is a
/// shift and a word, with no hash. 1 bit a memo of the store.
struct Marks(Vec<u64>);

impl Marks {
    fn new(end: u32) -> Marks { ... }
    fn insert(&mut self, memo: MemoId) -> bool { ... }
    fn contains(&self, memo: MemoId) -> bool { ... }
    fn iter(&self) -> impl Iterator<Item = MemoId> + '_ { ... }
}
```

In collection, before:

```rust
        let mut reached = Marks::new(self.store.end());
        let mut hot = Marks::new(self.store.end());
```

After:

```rust
        // alloc: one bitset a collection each, 1 bit a memo.
        let mut reached = BitSet::<MemoId>::new(self.store.end() as usize);
        let mut hot = BitSet::<MemoId>::new(self.store.end() as usize);
```

before:

```rust
                hot.insert(memo);
```

After:

```rust
                hot.insert_growing(memo);
```

before:

```rust
            if !reached.insert(memo) {
```

After:

```rust
            if !reached.insert_growing(memo) {
```

In reclaim, before:

```rust
        let mut held = Marks::new(self.store.end());
```

After:

```rust
        // alloc: one bitset a reclaim, 1 bit a memo.
        let mut held = BitSet::<MemoId>::new(self.store.end() as usize);
```

before:

```rust
                    held.insert(resolved.memo);
```

After:

```rust
                    held.insert_growing(resolved.memo);
```

(`hot.contains(memo)`, `reached.contains(memo)` and `held.contains(memo)` are unchanged.)

> forwarded to db (from requests, pass 16; told by tmux).

### `crates/blaze-query/src/tests.rs`

Imports: `Marks, ` leaves the `use crate::{ ... }` list, and the file gains
`use blaze_ids::bits::BitSet;` beside its other `use` lines.

`marks_are_a_bit_a_memo`, before:

```rust
    let mut marks = Marks::new(70);
    assert!(marks.insert(3));
    assert!(!marks.insert(3), "marked once");
    assert!(marks.insert(64));
    assert!(marks.insert(200), "past the end it was made for");
```

After:

```rust
    let mut marks = BitSet::<u32>::new(70);
    assert!(marks.insert_growing(3));
    assert!(!marks.insert_growing(3), "marked once");
    assert!(marks.insert_growing(64));
    assert!(marks.insert_growing(200), "past the end it was made for");
```

(the last two lines of the test are unchanged.)

Status: open

> applied by db (db-next3, 21:05): `Marks` is gone; collection and reclaim use `BitSet<MemoId>` with `insert_growing`, and the test is as written.

> forwarded to db (from requests, pass 16; told by tmux).

## dedup: bits: vendor/salsa `capture.rs` `Frame::old` onto `blaze_ids::bits::BitSet` (from dedup-bits-db)

From `dedup-bits-db` (report `/tmp/blaze-lanes/v2-dedup-bits-db.md`), for lane `dedup-bits`.

Why: `Frame::old` is a growable `u64`-word set of edge positions with its own bit logic; it is
`blaze_ids::bits::BitSet<u32>`. The hot path costs the same: `read_old` is one capacity compare,
then `insert` (one load, test and store, as before); `recycle` clears each bit it set with
`remove` instead of storing zero over its word, which is the same result because every bit set
in `old` is a position in `read`. The buffer is still grown once, to the frame's `base`, and
reused by later captures. `blaze-ids` depends on nothing of salsa's (blake3, blaze-radix, serde,
smallvec, thin-vec), so the edge is acyclic; salsa already has it as a dev-dependency.

> forwarded to db (from requests, pass 16; told by tmux).

### `vendor/salsa/Cargo.toml`

Move `blaze-ids` from the dev-dependencies to the dependencies. Before:

```toml
[dev-dependencies.blaze-ids]
path = "../../crates/blaze-ids"
```

After: that entry deleted, and beside the other `[dependencies.*]` entries (alphabetically,
before `[dependencies.salsa-macros]`'s neighbours as the file orders them):

```toml
[dependencies.blaze-ids]
path = "../../crates/blaze-ids"
```

(The tests that used it as a dev-dependency keep working: a normal dependency is visible to
tests.)

> forwarded to db (from requests, pass 16; told by tmux).

### `vendor/salsa/src/capture.rs`

Imports, before:

```rust
use std::fmt;

use crate::Revision;
```

After:

```rust
use std::fmt;

use blaze_ids::bits::BitSet;

use crate::Revision;
```

Field, before:

```rust
    /// Bits of the positions below `base` that `read` holds; all clear in a spare frame.
    old: Vec<u64>,
```

After:

```rust
    /// The positions below `base` that `read` holds; empty in a spare frame.
    old: BitSet<u32>,
```

`read_old`, before:

```rust
    fn read_old(&mut self, position: u32) {
        let (word, bit) = ((position / 64) as usize, 1u64 << (position % 64));
        if self.old.len() <= word {
            self.old.resize(self.base.div_ceil(64) as usize, 0);
        }
        if self.old[word] & bit == 0 {
            self.old[word] |= bit;
            self.read.push(position);
        }
    }
```

After:

```rust
    fn read_old(&mut self, position: u32) {
        if self.old.capacity() <= position as usize {
            self.old.grow(self.base as usize);
        }
        if self.old.insert(position) {
            self.read.push(position);
        }
    }
```

`recycle`, before:

```rust
            if position < self.base {
                self.old[(position / 64) as usize] = 0;
            }
```

After:

```rust
            if position < self.base {
                self.old.remove(position);
            }
```

In `Captures::open`, before:

```rust
                // alloc: `Vec::new` allocates nothing; a frame's buffers are reused by later captures
                old: Vec::new(),
```

After:

```rust
                // alloc: an empty set allocates nothing; a frame's buffers are reused by later captures
                old: BitSet::default(),
```

Test, before:

```rust
        assert!(inner.old.iter().all(|&word| word == 0));
```

After:

```rust
        assert!(inner.old.is_empty());
```

Status: open

> sent by db (db-next3, 21:05) to speed-salsa-next3 (window 22), which holds `vendor/salsa` now, with the `Sip` section above.

> forwarded to db (from requests, pass 16; told by tmux).

## 13. Forwards from db-matchck and db-unsafeck (db, 21:50)

Two of db-matchck's requests (`v2/requests/db-matchck.md`) are for the coordinator:
- 4 is already applied by requests-db-fixture. The matching hir arm is removed by db.
- 10: `#[derive(PartialEq, Eq)]` on minicore's `Option` (`crates/test-utils/src/minicore.rs:1609`).
  Without it, the `match_arm_statics` fixture is refused where rustc builds it.

Inside lane db: 11 (`StructuralPartialEq` from the expanded derive) is with db-expand-next2, and
7 (no borrow check of a refused body) with db-ty-next2.

db-unsafeck's requests (`v2/requests/db-unsafeck.md`):
- Diagnostics: 3, the six new `UnsafetyReason`s in `lower/missing_unsafe.rs`.
- Tools: 6.
- At integration: `impl_hir_database!` moves out of hir-ide along with its test dev-dependency.
- Inside lane db: 4 (`wrap_binder!` lowering) is with db-expand, and 5 (static-path bitset)
  with db-ty.

> forwarded to db (from requests, pass 17; told by tmux).

## frontend: frontend applies its own `## frontend-*` hooks in db's files (Andrew's decision, 2026-10-29 20:45)

Notice, not a request. Child `frontend-hooks` (window `frontend-hooks`) applies the unapplied
`## frontend-*` sections of this file and `db-expand.md` itself, one file at a time, and only
where no running db child owns the file or touched it in the last 20 minutes (else it tells that
child and leaves the section). Each section gets `> applied by frontend-hooks ...`, and a summary
of every edit goes at the end of this file for db to review in one read. The design asks of
"frontend: what frontend.md items 7-12 still need" stay db's.

> applied: nothing to apply (a notice); requests routes `## frontend-*` sections of db.md and db-expand.md to frontend-hooks from now on (requests, pass 19).

## dedup: bits: hir-ty-borrowck on frame words (supersedes the one above)

From `dedup-bits-db` (report `/tmp/blaze-lanes/v2-dedup-bits-db.md`), with
`mem-db-checks-check` (`v2/requests/mem-db-checks-check.md` B7.13, whose design this is).

Why: borrowck's `bitset.rs` is a copy of `blaze_ids::bits`, and mem-db-checks B2.1 moves its
words into the body's frame, because T1 (`a_clean_body_allocates_nothing`) and B7.10 need zero
global allocations for a clean body. Both hold if borrowck's sets are `blaze_ids::bits` sets
over frame words: `Bits<&'f mut [u64]>` (`BitSliceMut<'f>`) and
`BitMatrix<usize, &'f mut [u64]>` (the matrix's index type comes first).
`bitset.rs` keeps only the two aliases, the frame constructors and the `Transfer` trait; its own
bit logic is deleted.

**Order.** Apply after mem-db-checks B2 (`v2/requests/mem-db-checks.md`, B2.1 to B2.5 and the
call sites it lists), B7 (`v2/requests/mem-db-checks-check.md`), mem-db-checks-regions'
`known: BitMatrix<'f>`. `blaze_ids::bits::BitMatrix<I = usize, S = Vec<u64>>` already has its
storage parameter (`from_words(rows, columns, data: S)`, reads for `S: Words`, writes for
`S: WordsMut`, `Default` for `S: Default`). This section is written against the text those requests produce. Where it
differs from them, this section wins. Behavior is unchanged: the same members, the same panics
(`insert` and ranges past the words, `copy_from` of a set with a member past them), and
`contains` past the words is false.

> forwarded to db (db-ty-d-borrowck, in `crates/hir-ty-borrowck/`) (requests, pass 19; told by tmux).
> db-ty-d-borrowck (2026-10-29 20:59): not applied, waits on its own order: mem-db-checks B2
> and B7 and mem-db-checks-regions, none in the tree (no `crate::mem`, no `blaze-mem` dependency,
> `Regions<'db>` still). `bitset.rs` is unchanged until then.

### 1. `crates/hir-ty-borrowck/src/bitset.rs`: the whole file becomes

```rust
//! The analyses' sets: `blaze_ids::bits` sets over words in the body's frame, fixed in size
//! (no capacity word and no growth), so a clean body takes no global memory. Ranges are set
//! and cleared a word at a time, so a move path's subtree (a contiguous range of path ids)
//! costs a few word ops.

use std::ops::Range;

use blaze_ids::bits::{self as shared, BitSliceMut, Bits, WordsMut};
pub(super) use blaze_ids::bits::{range_masks, words_for};

use crate::mem::Frame;

/// A set of bits in the frame.
pub(super) type BitSet<'f> = BitSliceMut<'f>;

/// Rows of one width in the frame, a row found by one multiply: the entry sets of every block.
pub(super) type BitMatrix<'f> = shared::BitMatrix<usize, &'f mut [u64]>;

/// An empty set of `len` bits in `frame`.
pub(super) fn bits(frame: &Frame, len: usize) -> BitSet<'_> {
    Bits::from_words(frame.alloc_slice_fill_with(words_for(len), |_| 0))
}

/// The set of every bit below `len`, in `frame`.
pub(super) fn filled_bits(frame: &Frame, len: usize) -> BitSet<'_> {
    let mut out = bits(frame, len);
    out.insert_range(0..len);
    out
}

/// An empty set of a row's size of `matrix`, in `frame`, to copy its rows into.
pub(super) fn bits_for_rows_of<'f>(frame: &'f Frame, matrix: &BitMatrix<'_>) -> BitSet<'f> {
    Bits::from_words(frame.alloc_slice_fill_with(matrix.row_words(), |_| 0))
}

/// A copy of `set` in `frame`.
pub(super) fn bits_copy<'f>(frame: &'f Frame, set: &BitSet<'_>) -> BitSet<'f> {
    Bits::from_words(frame.alloc_slice_copy(set.words()))
}

/// An empty matrix of `rows` rows of `columns` bits in `frame`.
pub(super) fn matrix(frame: &Frame, rows: usize, columns: usize) -> BitMatrix<'_> {
    let data = frame.alloc_slice_fill_with(rows * words_for(columns), |_| 0);
    shared::BitMatrix::from_words(rows, columns, data)
}

/// What a statement does to a set: adds and removes bits, whatever is in it. Applied to a set
/// as a walk goes, or composed into a block's gen and kill sets
/// ([`crate::dataflow::GenKill::row`]), from the one function either way.
pub(super) trait Transfer {
    fn gen_range(&mut self, range: Range<usize>);
    fn kill_range(&mut self, range: Range<usize>);

    #[inline]
    fn gen_(&mut self, bit: usize) {
        self.gen_range(bit..bit + 1);
    }

    #[inline]
    fn kill(&mut self, bit: usize) {
        self.kill_range(bit..bit + 1);
    }
}

impl<S: WordsMut> Transfer for Bits<S> {
    #[inline]
    fn gen_range(&mut self, range: Range<usize>) {
        self.insert_range(range);
    }

    #[inline]
    fn kill_range(&mut self, range: Range<usize>) {
        self.remove_range(range);
    }
}

#[cfg(test)]
mod tests {
    use super::{Transfer, bits, matrix};

    #[test]
    fn a_set_and_a_matrix_take_no_global_memory() {
        blaze_mem::frame::task(blaze_mem::Tag::Mir, |frame| {
            let _ = bits(frame, 1);
            let _ = matrix(frame, 1, 1);
            blaze_alloc_count::assert_at_most(0, "a bitset and a matrix in the frame", || {
                let mut set = bits(frame, 1000);
                let mut rows = matrix(frame, 64, 1000);
                set.gen_range(3..900);
                rows.insert(63, 999);
                set.copy_from(&rows.row(63));
                assert!(set.contains(999) && !set.contains(3));
            });
        });
    }
}
```

(The first two allocations warm the frame's reservation, as in B2.1. B2.1's other tests of
`bitset.rs` are already in `crates/blaze-ids/src/bits/tests.rs`, against the same operations
over `Vec` words; drop them from this file.)

### 2. Spellings at every call site B2 and B7 write

Apply this rename across `crates/hir-ty-borrowck/src/` (B2's and B7's text uses the left
column):

| B2/B7 spelling | becomes |
|---|---|
| `BitSet::new(frame, n)`, `BitSet::new(ctx.frame, n)` | `bits(frame, n)`, `bits(ctx.frame, n)` |
| `BitSet::filled(frame, n)` | `filled_bits(frame, n)` |
| `BitSet::for_rows_of(frame, &m)` | `bits_for_rows_of(frame, &m)` |
| `a.clone_in(frame)` | `bits_copy(frame, &a)` |
| `BitMatrix::new(frame, r, c)` (also `self.ctx.frame`) | `matrix(frame, r, c)` |
| `set.load(m.row(r))` | `set.copy_from(&m.row(r))` (equal lengths, as B2.1 requires) |
| `set.union(&other)` on a `BitSet` | `set.union_with(&other)` (`IntervalSet::union` is unchanged) |
| `iter_words(m.row(r))` | `m.iter_row(r)` |
| `masks(range, ..)` | `range_masks(range, ..)` |
| `m.row(r)` / `m.row_mut(r)` where a `&[u64]` / `&mut [u64]` is wanted | `m.row(r).words()` / `m.row_mut(r).words_mut()` (they are `BitSlice`/`BitSliceMut` views now) |
| `BitSet<'f>`, `BitMatrix<'f>`, `BitSet::default()`, `BitMatrix::default()` | unchanged (the aliases; `&mut [u64]` is `Default`) |
| `insert`, `contains`, `remove`, `clear`, `first_in`, `iter`, `words`, `words_mut` | unchanged |

Imports: `bitset::{...}` lists drop `union_words` and `iter_words`, rename `masks` to
`range_masks`, and gain whichever of `bits`, `filled_bits`, `bits_for_rows_of`, `bits_copy` and
`matrix` the file calls.

Two places where the shared type returns a `bool` the old one didn't. A closure or match arm
that must be `()` gains braces. In `liveness.rs` `maybe_initialized_at_drops`:

```rust
        locals_in(moves, &state, |local| {
            exit.insert(index(block), local);
        });
```

and the same for `returned.insert(index(block), local)`. In `InitAtExit::restrict`,
`live.iter_mut().zip(mask)` becomes `live.iter_mut().zip(mask.words())`.

### 3. `crates/hir-ty-borrowck/src/dataflow.rs`: the scratch rows become frame sets

`forward`, from `let mut entry` to the end of the loop (B2.3's text), becomes:

```rust
    let mut entry = matrix(frame, count, blocks.words * 64);
    if count == 0 {
        return entry;
    }
    entry.row_mut(index(start)).copy_from(boundary);
    let mut out = bits(frame, blocks.words * 64);
    let mut along = bits(frame, blocks.words * 64);
    let mut queue = WorkQueue::new(frame, count, cfg.order.iter().copied());
    while let Some(at) = queue.pop() {
        out.copy_from(&entry.row(index(at)));
        blocks.apply(index(at), out.words_mut());
        for &to in cfg.successors(at) {
            along.copy_from(&out);
            edge(at, to, along.words_mut());
            if entry.union_row_with(index(to), &along) {
                queue.push(to);
            }
        }
    }
```

`backward`, likewise:

```rust
    let mut entry = matrix(frame, count, blocks.words * 64);
    let mut exit = bits(frame, blocks.words * 64);
    let mut along = bits(frame, blocks.words * 64);
    let mut queue = WorkQueue::new(frame, count, cfg.order.iter().rev().copied());
    while let Some(at) = queue.pop() {
        exit.clear();
        for &to in cfg.successors(at) {
            along.copy_from(&entry.row(index(to)));
            edge(at, to, along.words_mut());
            exit.union_with(&along);
        }
        blocks.apply(index(at), exit.words_mut());
        if entry.row(index(at)).words() != exit.words() {
            entry.row_mut(index(at)).copy_from(&exit);
```

(the predecessor loop after it is unchanged). `WorkQueue::queued` is `bits(frame, blocks)`.
In the tests, `rows(entry: &crate::bitset::BitMatrix<'_>, ..)` becomes
`(0..blocks).map(|b| entry.iter_row(b).collect()).collect()`.

### 4. `crates/hir-ty-borrowck/src/liveness.rs` `live_ranges`

B2's `let mut live = BitSet::new(frame, locals);` and its frame `along` slice become:

```rust
    let mut live = bits(frame, locals);
    let mut along = bits(frame, locals);
    for &block in &cfg.order {
        live.clear();
        for &successor in cfg.successors(block) {
            along.copy_from(&entry.row(index(successor)));
            edge(block, successor, along.words_mut());
            live.union_with(&along);
        }
```

### 5. `crates/hir-ty-borrowck/src/constraints.rs` `from_closure`

B2 keeps it as `FVec<'f, u64>`, growing in the frame. A frame `Vec` isn't a `GrowWords` storage
(`blaze_ids` has no allocator parameter), so the words stay in the `FVec`, and the bit logic
goes through a view. In `push`, before:

```rust
        if it.from_closure {
            self.from_closure[at / 64] |= 1 << (at % 64);
        }
```

After (the `is_multiple_of(64)` push of a zero word before it stays):

```rust
        BitSliceMut::from_words(&mut self.from_closure[..]).set(at, it.from_closure);
```

In `get`, before:

```rust
            from_closure: self.from_closure[at / 64] & (1 << (at % 64)) != 0,
```

After:

```rust
            from_closure: BitSlice::from_words(&self.from_closure[..]).contains(at),
```

with `use blaze_ids::bits::{BitSlice, BitSliceMut};`.

Status: open

## frontend-db: stop every reader through `&Db` (`Database::stop_readers`; 2026-10-29)

Why: `base_db::Db` has no `Clone`, so blaze-engine's three places that cancelled readers by
writing to the owner of a database fork (`lower_until`, `compile_tiered`, `run_holding`'s detached
builds, through `Builds::settle`; frontend-db-share's report) need a stop that works while readers
hold `&Db`. A task's own `CancellationToken` isn't enough: `DisableLocalCancellationGuard` turns
it off inside every cycle head, and inference and MIR building, which take the time, run there.
Upstream salsa's pending-write flag was checked at the same point and was never disabled in a
cycle. This brings it back, set through `&self` and cleared by the next write.
`tests/parallel/cancellation_in_fixpoint.rs` already shows that a `PendingWrite` thrown inside a
fixpoint, followed by `trigger_cancellation`, leaves nothing reusable behind. base-db's
`Db::stop_readers`/`readers_stopped` and `base_db::Shared` (frontend-db, `crates/base-db/src/db/shared.rs`)
call these two methods and are complete once they exist.

1. `vendor/salsa/src/runtime.rs`, the struct: after `cancellation_count: AtomicU8,` add
   ```rust
       /// Set by [`Database::stop_readers`](crate::Database::stop_readers): every query that
       /// starts from now unwinds with [`Cancelled::PendingWrite`], cycle heads included, until
       /// the next write clears it. Read on every query call next to `revisions[0]`.
       #[cfg_attr(feature = "persistence", serde(skip))]
       readers_stopped: AtomicBool,
   ```
   (`AtomicBool` to the `std::sync::atomic` import.) In `Default`: `readers_stopped: Default::default(),`.
   Methods, after `bump_cancellation_count`:
   ```rust
       #[inline]
       pub(crate) fn readers_stopped(&self) -> bool {
           self.readers_stopped.load(Ordering::Relaxed)
       }

       pub(crate) fn stop_readers(&self) {
           self.readers_stopped.store(true, Ordering::Relaxed);
       }

       pub(crate) fn clear_stop(&mut self) {
           *self.readers_stopped.get_mut() = false;
       }
   ```
   `Relaxed` is enough: the flag orders nothing else; the write that follows is ordered by the
   `&mut` the writer gets only once every reader has let go (an `Arc` count or a scope's join).
2. `vendor/salsa/src/storage.rs`, `prepare_write`: after `let zalsa = &mut *self.zalsa_impl;` add
   `zalsa.runtime_mut().clear_stop();`. The epoch bump that follows is what tells apart the
   provisional memos the stopped readers left behind (as in `cancellation_in_fixpoint.rs`).
3. `vendor/salsa/src/zalsa.rs`, `unwind_if_revision_cancelled`: before
   `if zalsa_local.should_trigger_local_cancellation() {` add
   ```rust
           if self.runtime.readers_stopped() {
               Cancelled::PendingWrite.throw();
           }
   ```
   (`use crate::Cancelled;` if not in scope.) Cost on a memo hit: one `Relaxed` load from
   `Runtime`, which the hit reads anyway for `current_revision`, and a never-taken branch:
   about 0.3 ns against the hit's 6-8 ns (`docs/v2/plan/lanes/frontend/db.md`, speed of light).
4. `vendor/salsa/src/database.rs`, after `trigger_cancellation`:
   ```rust
       /// Has every query running on this database, on any thread and inside cycles too, unwind
       /// with [`Cancelled::PendingWrite`] at its next query call, and every query started
       /// after it as well, until the next write (`&mut`, [`Self::trigger_cancellation`]
       /// included) clears it. For an owner that must write while other threads read through
       /// `&Self`: it stops them, waits for them to let go, then writes.
       fn stop_readers(&self) {
           self.zalsa().runtime().stop_readers();
       }

       /// Whether [`Self::stop_readers`] was called since the last write.
       fn readers_stopped(&self) -> bool {
           self.zalsa().runtime().readers_stopped()
       }
   ```
   If `Zalsa` has no `runtime()` accessor, add `#[inline] pub(crate) fn runtime(&self) -> &Runtime { &self.runtime }`.
5. `vendor/salsa/src/cancelled.rs`, the `PendingWrite` doc: replace "Salsa never throws it: a
   write takes the database by `&mut`, so no query runs during one. Hosts throw it to abandon
   work for a write they are about to make." with "Thrown at a query call after
   [`Database::stop_readers`](crate::Database::stop_readers), until the next write; hosts also
   throw it to abandon work for a write they are about to make."
6. Test, `vendor/salsa/tests/parallel/stop_readers.rs` (and `mod stop_readers;` in
   `tests/parallel/main.rs`), next to `cancellation_in_fixpoint.rs` and using its setup:
   ```rust
   // Shuttle doesn't like panics inside of its runtime.
   #![cfg(not(feature = "shuttle"))]

   //! `stop_readers` reaches a query inside a fixpoint cycle, where a task's own token is
   //! ignored, and the next write clears it, leaving no provisional memo to reuse.

   use std::sync::atomic::{AtomicBool, Ordering};

   use salsa::{Cancelled, Database};

   use crate::setup::{Knobs, KnobsDatabase};

   static STOP: AtomicBool = AtomicBool::new(false);

   #[salsa::input]
   struct Input {
       #[returns(copy)]
       value: u32,
   }

   #[salsa::tracked(returns(copy), cycle_fn = cycle_fn, cycle_initial = cycle_initial)]
   fn cycle_a(db: &dyn KnobsDatabase, input: Input) -> u32 {
       let value = cycle_b(db, input);
       if STOP.swap(false, Ordering::Relaxed) {
           db.cancellation_token().cancel();
           db.stop_readers();
       }
       inside(db, input);
       value
   }

   #[salsa::tracked(returns(copy), cycle_fn = cycle_fn, cycle_initial = cycle_initial)]
   fn cycle_b(db: &dyn KnobsDatabase, input: Input) -> u32 {
       let value = cycle_a(db, input);
       value.saturating_add(1).min(input.value(db))
   }

   #[salsa::tracked]
   fn inside(db: &dyn KnobsDatabase, input: Input) {
       input.value(db);
   }

   fn cycle_initial(_db: &dyn KnobsDatabase, _id: salsa::Id, _input: Input) -> u32 {
       0
   }

   fn cycle_fn(
       _db: &dyn KnobsDatabase,
       _cycle: &salsa::Cycle,
       _last_provisional_value: &u32,
       value: u32,
       _input: Input,
   ) -> u32 {
       value
   }

   #[test]
   fn stop_readers_unwinds_a_cycle_head_and_a_write_clears_it() {
       let mut db = Knobs::default();
       let input = Input::new(&db, 3);
       STOP.store(true, Ordering::Relaxed);
       assert!(matches!(Cancelled::catch(|| cycle_a(&db, input)), Err(Cancelled::PendingWrite)));
       assert!(db.readers_stopped());
       assert!(matches!(Cancelled::catch(|| inside(&db, input)), Err(Cancelled::PendingWrite)));
       db.trigger_cancellation();
       assert!(!db.readers_stopped());
       assert_eq!(cycle_a(&db, input), 3);
   }
   ```
   The token is cancelled too, to show which of the two stops the cycle: the token alone is
   disabled there, so a `Cancelled::Local` would mean the guard leaked.

Status: open

> applied by frontend-hooks (Andrew's decision 2026-10-29 20:45): §1-6 as written in
> `vendor/salsa` (files untouched for 46+ min; speed-salsa's window is gone, db-salsa has none).
> `runtime.rs`: `readers_stopped: AtomicBool` (serde skip, `Default`), `readers_stopped`,
> `stop_readers`, `clear_stop` after `bump_cancellation_count`; `storage.rs` `prepare_write` clears
> it first; `zalsa.rs` `unwind_if_revision_cancelled` throws `crate::Cancelled::PendingWrite`
> before the local check (`Zalsa::runtime()` existed); `database.rs` `stop_readers` and
> `readers_stopped` after `trigger_cancellation`; `cancelled.rs` the `PendingWrite` doc; test
> `tests/parallel/stop_readers.rs` and `mod stop_readers;` in `tests/parallel/main.rs`. Checked
> the test's last call: the thread's token is reset when the outermost attach guard drops on the
> unwind (`attach.rs:88`), so `cycle_a` runs clean after the write.

> forwarded to frontend-hooks (a `## frontend-*` section of db.md, db.md's 20:45 notice) (requests, pass 20; told by tmux).

## frontend-hooks: applied (for db)

Every edit frontend-hooks made in db's files under Andrew's decision of 2026-10-29 20:45
(frontend applies its own `## frontend-*` hooks where no running db child owns or just touched
the file). Nothing was built. Each section above carries its own `>` line.

| File | What | Section |
|---|---|---|
| `crates/hir-def/Cargo.toml` | `blaze-query.workspace = true` in `[dependencies]` | frontend-nameres-core §8 |
| `crates/hir-def/Cargo.toml` | `proptest.workspace = true` in `[dev-dependencies]` | frontend-nameres-graph |
| `crates/hir-def/src/lib.rs` | `pub mod deps;` after `pub mod nameres;` | frontend-deps |
| `crates/hir-def/src/signatures.rs` | `served_*` hook at the head of the ten `of`s | frontend-deps-sigs §2 |
| `crates/hir-def/src/signatures.rs` | `EnumVariants::of` = hook + `Self::lower`; old body in `lower(db: &Db, e)` | frontend-deps-sigs §3 |
| `crates/hir-def/src/signatures.rs` | `VariantFields::from_parts`, `EnumVariants::from_parts` | frontend-deps-sigs §4 |
| `crates/base-db/src/lib.rs` | `FileText` field `text: syntax::Text`; `text()` returns `&syntax::Text` | frontend-text (Text §2; line index §3) |
| `crates/base-db/src/files.rs` | `write_text` takes `Option<(syntax::Text, ContentId, ContentId)>`, no copy; `set_text` makes `Text::from(&str)`; empty text `Text::from_static("")` | same |
| `crates/base-db/src/reparse.rs` | D8's slot and `Reparse::{new, after, parse}`, `whole`, `assert_whole` on `Text`; `ptr_eq` check | same (re-derived for D8) |
| `crates/base-db/src/view.rs` | `ViewFile { path, id, bytes: &blaze_vfs::Bytes }`; `plan_view`/`write_view` read `id`/`bytes`; texts are `blaze_vfs::text_of(bytes.clone())` | same |
| `crates/base-db/src/tests.rs` | `vfs_bytes`/`view_file` at the view sites; `tree.source().ptr_eq(..)`; new `a_views_text_is_its_bytes` | same |
| `crates/base-db/src/budget_tests.rs` | views of `blaze_vfs::Bytes`; `PER_CHANGED_FILE` 2 -> 1 (no text copy) | same |
| `vendor/salsa/src/{runtime,storage,zalsa,database,cancelled}.rs` | `Database::stop_readers`/`readers_stopped`, cleared by `prepare_write`, thrown in `unwind_if_revision_cancelled` | frontend-db: stop every reader |
| `vendor/salsa/tests/parallel/{stop_readers,main}.rs` | the test and its `mod` line | same |

Left, with who has it:
- db-expand-names-next2 (running; `nameres*`, `item_scope.rs`): frontend-nameres-core §1-7,
  frontend-nameres-impls, frontend-nameres-core-macros `of_scope`, frontend-deps-sigs §1
  (`frozen.rs` `Enc`/`Dec` to `pub(crate)`; `deps/signatures/table.rs` needs it). Told.
- db-expand-macros-next2 (running; `hir-expand`): frontend-text line index §2, the four
  frontend-tokens sections and frontend-deps-reparse's `Attr::input` (db-expand.md), and the new
  `FileRange::file_text` return type (db-expand.md, "frontend-hooks: ..."). Told.
- db: frontend-image §5-6 (design), the `compile_fail` choice, items 7-12 (design).
- Not applied, superseded: frontend-deps-reparse's `parse_file` (db's D8 does item 14).
- Outside db, sent on: format's `BodySyntax::text` (`v2/requests/format.md`), load-cargo's
  `ViewFile` site (cargo-inbox told). Until they land, `hir-def/src/expr_store/body.rs`,
  `hir-expand/src/files.rs` and `load-cargo/src/v2.rs` don't compile against base-db.

> applied: nothing to apply (frontend-hooks' summary for db) (requests, pass 21).

## 14. Coordinator: reword `docs/v2/plan/design/frontend.md` item 9 (db, 21:15)

Andrew's decision (21:10): cancellation keeps the unwind, but only to carry a stop from a query
call up to the task it runs in. Item 9's last sentence, before:

```text
   be unwind-safe, and cancelling costs an unwind through deep stacks. v2 cancels
   cooperatively: the plane stops scheduling a cancelled request's tasks, and a running task
   sees the flag at its next query call and returns.
```

After:

```text
   be unwind-safe, and cancelling costs an unwind through deep stacks. v2 cancels
   cooperatively: the plane stops scheduling a cancelled request's tasks, and a running task
   sees the flag at its next query call and unwinds to its own task boundary, where it is
   caught once (`base_db::Db::run_all`, `base_db::Shared`). Nothing is caught per query, and
   a query never returns a `Result` for it: a cancel comes once per edit made during a build,
   and a hit pays no branch for it.
```

Status: open

> forwarded to the coordinator (`docs/v2/plan/design/`), on the requests report's list (requests, pass 21).

## 15. format: hir-def's persisted names stay portable (`crates/hir-def/src/expr_store/persist.rs`; db, 21:15)

From `## frontend-image` §5 (answered above). A store's encoding is the one that dependency
signature images carry (`hir_def::deps::signatures::table`, `StoreRoot`). The store keeps those
images and shares them between daemons, and each daemon numbers its symbols its own way. So
`blaze_format::Sym(symbol.id())` must not appear in them. Keep the name as text (`Str`), or, if
reading one costs too much, give each image buffer its own name table: names as `u32` indexes
into it, read in place, each interned once when the image is opened (one probe per distinct
name per image, not per read). Either way the fingerprint of a store is over text, never over a
process's symbol ID.

Status: open

> forwarded to mem-format (mem-format-hir-apply, which holds `hir-def/src/expr_store/`; format is DONE) (requests, pass 21; told by tmux).
> applied by mem-format-hir-apply (2026-10-29): already true in the tree, and now held by a test.
> hir-def writes no `blaze_format::Sym` and no `Symbol::id()` (`rg` over `crates/hir-def/src`;
> the only `Sym` there is `AsmOperand::Sym`). Names are written two ways, both text:
> - `Code for Name`/`Symbol` (`hir/persist.rs` `Words::symbol`) write a `u32` index into the
>   buffer's own name table, which holds each text once, in first-use order.
> - A record field's or field pattern's name (`FieldNames`, `expr_store/persist.rs`) is a `Str`
>   into the buffer's field text.
> So a store's bytes, and its fingerprint, are over text. Reads intern the text at each read
> (`Read::symbol`), which is the "keep the name as text" option. Interning once per image open
> would need a per-image table of symbols and belongs to `hir/persist.rs`, which
> frontend-deps-sigs holds. Test: `names_are_written_as_text` (`expr_store/persist.rs`).
> Status: applied.

## dedyn: crates/blaze-query/src/lib.rs: `KeySource::Typed` stays `dyn`, its comment says why it is cold (2026-10-29)

Checked: the `&dyn Any` is downcast only in `run_query`, i.e. when a query runs (`execute`, in
`resolve_held` after every kept memo failed to verify, and `value_of` refilling an evicted
value). A lookup that finds its memo (`state.results`, a verified memo) passes the reference on
and never touches the vtable. A run also writes and fingerprints the value, so one `TypeId`
compare is noise there, and the `Runner` table (fn pointers, kinds registered at run time) is
why the key is erased. No code change; only the comment, which today splits its reason from the
`dyn:` tag:

Before (lines 298–300):

```rust
    // Kinds register at run time, so a typed key is erased once a run and read back by one
    // dyn: `TypeId` compare.
    Typed(&'a (dyn Any + Send + Sync)),
```

After:

```rust
    // dyn: kinds register at run time into a table of `Runner` fn pointers, so the caller's
    // key is erased; read back by one `TypeId` compare when the query runs, never on a lookup
    // that finds its memo.
    Typed(&'a (dyn Any + Send + Sync)),
```

Lines 1609, 1646 and 1747 (`// dyn: see `KeySource::Typed``) stay.

> forwarded to db (`crates/blaze-query/`; told db-next3 by tmux; requests, pass 22).

## frontend-bodies: R1, a def map's records leave the `DefMap` value (2026-10-29)

From `frontend-fast-bodies` (notes: `docs/v2/plan/lanes/frontend/fast-bodies.md`, section 1).
Cc `frontend-nameres` and `frontend-deps` (they touched `facts/` and `frozen.rs`).

Why: every resolver and every `DbInterner::new_with` reads the crate's whole `DefMap` (one
tracked field of `DefMapPair`). Its derived `PartialEq` covers `names` (every `use` changes
it), `diagnostics`, `diagnostics_by_module` and `derive_helpers_in_scope` (every derived item).
So adding a `use` or a `#[derive]` anywhere in a crate re-infers every body of it. No
resolution reads these four; they move to a tracked struct of their own at
`hand_over_scopes`, as the modules and scopes already do, and the `DefMap` keeps its id.

### `crates/hir-def/src/nameres.rs`

1. In `struct DefMap`, before:

```rust
    /// `diagnostics` grouped by module, made once collection is done ([`Self::hand_over_scopes`]).
    diagnostics_by_module: DiagnosticsByModule,

    /// The crate data that is shared between a crate's def map and all its block def maps.
    data: Arc<DefMapCrateData>,
}
```

After:

```rust
    /// `diagnostics` grouped by module, made once collection is done ([`Self::hand_over_scopes`]).
    diagnostics_by_module: DiagnosticsByModule,
    /// The [`HeldRecords`] that `names`, `diagnostics`, `diagnostics_by_module` and
    /// `derive_helpers_in_scope` move to when collection is done, leaving them empty: a `use` or a
    /// derive added changes the records and leaves this value equal, so what reads the `DefMap`
    /// (every resolver, every solver) is not run again for it.
    records: Option<salsa::Id>,

    /// The crate data that is shared between a crate's def map and all its block def maps.
    data: Arc<DefMapCrateData>,
}

type DeriveHelpers =
    FxHashMap<AstId<ast::Item>, Vec<(Name, MacroId, Either<MacroCallId, BuiltinDeriveImplId>)>>;

/// What a collected def map recorded that no resolution reads: how its imports and macro calls
/// resolved, its diagnostics and the derive helpers in scope of its items.
#[salsa::tracked]
struct HeldRecords<'db> {
    root: ModuleIdLt<'db>,
    #[tracked]
    #[returns(ref)]
    records: OwnedRecords,
}

#[derive(Debug, Default, PartialEq, Eq)]
struct OwnedRecords {
    names: uses::NameRecords,
    diagnostics: Vec<DefDiagnostic>,
    diagnostics_by_module: DiagnosticsByModule,
    derive_helpers_in_scope: DeriveHelpers,
}

/// A def map's records wherever they are ([`DefMap::records`]).
#[derive(Clone, Copy)]
pub struct Records<'a> {
    pub names: &'a uses::NameRecords,
    pub diagnostics: &'a [DefDiagnostic],
    diagnostics_by_module: &'a DiagnosticsByModule,
    derive_helpers_in_scope: &'a DeriveHelpers,
}
```

(and the field's own type in the struct, `derive_helpers_in_scope:
FxHashMap<AstId<ast::Item>, Vec<(Name, MacroId, Either<MacroCallId, BuiltinDeriveImplId>)>>,`,
becomes `derive_helpers_in_scope: DeriveHelpers,`).

2. `DefMap::empty`'s literal, before `            diagnostics_by_module: DiagnosticsByModule::default(),
            data: crate_data,`, after:

```rust
            diagnostics_by_module: DiagnosticsByModule::default(),
            records: None,
            data: crate_data,
```

3. `shrink_to_fit`'s exhaustive `let Self { .. }`: add `records: _,` after `names: _,`.

4. `hand_over_scopes`, before:

```rust
        let ModulesSlot::Building(modules) = &mut self.modules else { return };
        self.diagnostics_by_module = DiagnosticsByModule::of(&self.diagnostics);
```

After:

```rust
        let ModulesSlot::Building(modules) = &mut self.modules else { return };
        let diagnostics = std::mem::take(&mut self.diagnostics);
        let records = OwnedRecords {
            names: std::mem::take(&mut self.names),
            diagnostics_by_module: DiagnosticsByModule::of(&diagnostics),
            diagnostics,
            derive_helpers_in_scope: std::mem::take(&mut self.derive_helpers_in_scope),
        };
        let held = HeldRecords::new(db, self.root, records);
        self.records = Some(salsa::plumbing::AsId::as_id(&held));
```

(`shrink_to_fit` runs before `hand_over_scopes` at both call sites in `facts/driver.rs`, lines
726 and 1055, so the moved vectors are already shrunk.)

5. The accessors. Before:

```rust
    pub fn derive_helpers_in_scope(
        &self,
        id: AstId<ast::Adt>,
    ) -> Option<&[(Name, MacroId, Either<MacroCallId, BuiltinDeriveImplId>)]> {
        self.derive_helpers_in_scope.get(&id.map(|it| it.upcast())).map(Deref::deref)
    }

    /// What the paths of this def map's imports and macro calls resolved through, and how its
    /// macro calls and derive helpers resolved.
    pub fn name_records(&self) -> &uses::NameRecords {
        &self.names
    }
```

After:

```rust
    /// This def map's records: its own while it is collected, the database's once handed over,
    /// so that a read of them depends on them and not on the `DefMap`.
    pub fn records<'a>(&'a self, db: &'a Db) -> Records<'a> {
        match self.records {
            Some(id) => {
                let held = <HeldRecords<'a> as salsa::plumbing::FromId>::from_id(id).records(db);
                Records {
                    names: &held.names,
                    diagnostics: &held.diagnostics,
                    diagnostics_by_module: &held.diagnostics_by_module,
                    derive_helpers_in_scope: &held.derive_helpers_in_scope,
                }
            }
            None => Records {
                names: &self.names,
                diagnostics: &self.diagnostics,
                diagnostics_by_module: &self.diagnostics_by_module,
                derive_helpers_in_scope: &self.derive_helpers_in_scope,
            },
        }
    }

    pub fn derive_helpers_in_scope<'a>(
        &'a self,
        db: &'a Db,
        id: AstId<ast::Adt>,
    ) -> Option<&'a [(Name, MacroId, Either<MacroCallId, BuiltinDeriveImplId>)]> {
        self.records(db).derive_helpers_in_scope.get(&id.map(|it| it.upcast())).map(Deref::deref)
    }

    /// What the paths of this def map's imports and macro calls resolved through, and how its
    /// macro calls and derive helpers resolved.
    pub fn name_records<'a>(&'a self, db: &'a Db) -> &'a uses::NameRecords {
        self.records(db).names
    }
```

Before:

```rust
    pub fn diagnostics(&self) -> &[DefDiagnostic] {
        self.diagnostics.as_slice()
    }

    /// The diagnostics of `module` ([`DefDiagnostic::in_module`]), in the order of
    /// [`Self::diagnostics`]: a binary search, not a scan of them all.
    pub fn diagnostics_of(
        &self,
        module: ModuleId,
    ) -> impl ExactSizeIterator<Item = &DefDiagnostic> + '_ {
        self.diagnostics_by_module
            .of_module(module)
            .iter()
            .map(|&at| &self.diagnostics[at as usize])
    }
```

After:

```rust
    pub fn diagnostics<'a>(&'a self, db: &'a Db) -> &'a [DefDiagnostic] {
        self.records(db).diagnostics
    }

    /// The diagnostics of `module` ([`DefDiagnostic::in_module`]), in the order of
    /// [`Self::diagnostics`]: a binary search, not a scan of them all.
    pub fn diagnostics_of<'a>(
        &'a self,
        db: &'a Db,
        module: ModuleId,
    ) -> impl ExactSizeIterator<Item = &'a DefDiagnostic> + 'a {
        let records = self.records(db);
        records
            .diagnostics_by_module
            .of_module(module)
            .iter()
            .map(move |&at| &records.diagnostics[at as usize])
    }
```

### `crates/hir-def/src/nameres/facts/driver.rs`, `facts/macros.rs`, `facts/unit.rs`

Each `DefMap { .. }` literal (driver.rs about line 663, macros.rs 957, unit.rs 248): after its
`diagnostics_by_module: DiagnosticsByModule::default(),` line add `records: None,`.

### `crates/hir-def/src/nameres/frozen.rs`, `fn def_map` (encoder, line 1817)

Before:

```rust
            derive_helpers_in_scope,
            diagnostics,
            diagnostics_by_module: _,
            data,
            session_macros,
            names,
        } = it;
        let db = self.db;
```

After:

```rust
            derive_helpers_in_scope: _,
            diagnostics: _,
            diagnostics_by_module: _,
            records: _,
            data,
            session_macros,
            names: _,
        } = it;
        let db = self.db;
        let Records { names, diagnostics, derive_helpers_in_scope, .. } = it.records(db);
```

(and `Records` joins the `super::` / `crate::nameres::` import of `DefMap` there). The decoder
(line 3193, 3227–3228) keeps setting the fields: `decode` calls `hand_over_scopes` after, which
moves them. Test at line 3495: `main(&live).name_records()` becomes
`main(&live).name_records(&live)`.

### `crates/hir-def/src/nameres/facts/tests.rs`, `fn project` (line 861)

In the `let DefMap { .. } = def_map;`: `derive_helpers_in_scope,` → `derive_helpers_in_scope: _,`,
`diagnostics,` → `diagnostics: _,`, `diagnostics_by_module,` → `diagnostics_by_module: _,`, and
add `records: _,`. After `} = def_map;` add:

```rust
    let Records { diagnostics, diagnostics_by_module, derive_helpers_in_scope, .. } =
        def_map.records(db);
```

(`Records`'s private fields are visible here, a child module of `nameres`.)

### Call sites taking `db` now (each line exactly)

- `hir-def/src/nameres/tests/worklist.rs:26`: `def_map.diagnostics()` → `def_map.diagnostics(&db)`.
- `hir-def/src/nameres/tests/uses.rs:91`: `def_map.name_records();` → `def_map.name_records(db);`.
- `tests/uses.rs:325, 351, 379`: `def_map.name_records()` → `def_map.name_records(&db)`.
- `tests/uses.rs:386`: `def_map.diagnostics()` → `def_map.diagnostics(&db)`.
- `tests/batch_order.rs:15`: `def_map.diagnostics()` → `def_map.diagnostics(&db)`.
- `facts/driver.rs:1381`: `new.diagnostics()` → `new.diagnostics(&db)`.
- `facts/driver.rs:1430–1431`: `.def_map(&db).name_records()` → `.def_map(&db).name_records(&db)`.
- `hir-ide/src/tests/consteval/const_macro.rs:279`: `def_map.diagnostics()` → `def_map.diagnostics(db)`.
- `hir/src/diagnostics.rs:1754`: `def_map.diagnostics_of(def)` → `def_map.diagnostics_of(self.db, def)`.
- `hir/src/semantics.rs:691, 714`: `.derive_helpers_in_scope(InFile::new(sa.file_id, id))?` →
  `.derive_helpers_in_scope(self.db, InFile::new(sa.file_id, id))?`.
- `hir/src/semantics.rs:1488`: `.derive_helpers_in_scope(InFile::new(expansion, id))?;` →
  `.derive_helpers_in_scope(db, InFile::new(expansion, id))?;`.
- `hir/src/source_analyzer.rs:1383`: `.derive_helpers_in_scope(InFile::new(self.file_id, ast_id))`
  → `.derive_helpers_in_scope(db, InFile::new(self.file_id, ast_id))`.

Test: `crates/hir/src/tests/bodies.rs` (below) `a_use_reinfers_no_body`,
`a_derived_item_reinfers_no_body`.

> frontend-db (2026-10-29): no objection. Its new accessors (`records`, `name_records`,
> `diagnostics`) take `&Db`, as every query does now; nothing of the `&Db` migration touches
> `frozen.rs`'s def-map encoder or `deps/` images.

## frontend-bodies: R2, `crate_recursion_limit` firewall for the interner (2026-10-29)

Why: `DbInterner::new_with` reads the whole `DefMap` for one `u32`, so every body and signature
that solves depends on it. Even with R1 the `DefMap` changes with the crate's features and
prelude; the limit alone changes only with `#![recursion_limit]`.

`crates/hir-def/src/nameres.rs`, after `pub fn crate_def_map`:

```rust
/// The recursion limit of `krate` (`#![recursion_limit]`, 128 by default): what the solver
/// reads of the def map, so that it depends on that alone.
#[salsa::tracked]
pub fn crate_recursion_limit(db: &Db, krate: Crate) -> u32 {
    crate_def_map(db, krate).recursion_limit()
}
```

`crates/hir-ty/src/next_solver/interner.rs:436`, before:

```rust
            recursion_limit: hir_def::nameres::crate_def_map(db, krate).recursion_limit(),
```

After:

```rust
            recursion_limit: hir_def::nameres::crate_recursion_limit(db, krate),
```

## frontend-bodies: R3, `probe_all_traits` reads one bucket per crate (2026-10-29)

Why: `infer/sized_bound.rs` `probe_all_traits` (rustc's `ProbeScope::AllTraits`, for
`MethodCallIllegalSizedBound`) walks every module scope of every crate, so a body with that
error re-infers on any item added anywhere. `in_crate_trait_value_items(db, krate, name)` is
the same set (traits declared in a module, not a block, with a fn or const item named `name`),
already a per-name bucket. It needs `pub(crate)` in `method_resolution.rs` (asked of
`frontend-fast-method`, owner of that file, in `v2/requests/frontend-fast-method.md`).

`crates/hir-ty/src/infer/sized_bound.rs`, before:

```rust
        let has_value_item = |trait_: TraitId| {
            trait_.trait_items(db).items.iter().any(|(item_name, item)| {
                *item_name == name
                    && matches!(item, AssocItemId::FunctionId(_) | AssocItemId::ConstId(_))
            })
        };
        for &krate in all_crates(db).iter() {
            for (_, module) in crate_def_map(db, krate).modules(db) {
                traits.extend(module.scope(db).declarations().filter_map(|def| match def {
                    ModuleDefId::TraitId(it) if has_value_item(it) => Some(it),
                    _ => None,
                }));
            }
        }
```

After:

```rust
        for &krate in all_crates(db).iter() {
            traits.extend(in_crate_trait_value_items(db, krate, name).iter().map(|&(it, _)| it));
        }
```

Imports: `AssocItemId`, `ModuleDefId` and `nameres::crate_def_map` leave the `hir_def::` list;
`in_crate_trait_value_items` joins the `method_resolution::{..}` list.

## frontend-bodies: `mod bodies;` in `crates/hir/src/tests.rs` (2026-10-29)

After line 3 `mod infer;` add `mod bodies;` (sorted: before `mod db;`, i.e. lines 1–4 become
`mod api;`, `mod bodies;`, `mod db;`, `mod infer;`, `mod lower;`). The file
`crates/hir/src/tests/bodies.rs` is new and mine: incremental tests that no edit of one kind
re-infers another body.

## frontend-deps: `ExpressionStoreRef::decode_in`, borrowing a dependency image (frontend.md item 5; 2026-10-29)

Forwarded from `v2/requests/speed-hir-store.md` ("For frontend-deps-sigs: decode a signature's
store borrowing the image"): speed-hir-store is DONE without it, and `hir-def` is yours.
`crates/hir-def/src/deps/signatures.rs` `read` already calls
`ExpressionStore::read(store)?.decode_in(image)?` for `StoreBytes::InPlace`, so hir-def does not
compile until this lands. `Moved` stores keep `decode()`.

The owner type has changed from the one in that request: images are now read in place from the
store (`blaze_cas::Reader::pinned`, a mapping), so the owner is
`crate::deps::ImageBuf = Arc<dyn crate::deps::Held>`, not `Arc<Aligned>`. Its
`bytes()` stay at the same address, unchanged, while any count lives, and start at a
16-byte boundary.

1. `crates/hir-def/src/hir/col.rs`: a `Col` may hold a count of an image:
   - `owner: Option<SlabBytes>` becomes an owner that is a slab or an `ImageBuf`. The invariant
     gains a third case: `owner` is an image, `T: Copy`, and the elements lie inside
     `owner.bytes()`.
   - `pub(crate) fn borrowed(owner: &crate::deps::ImageBuf, elements: &[T]) -> Option<Col<T>>`
     where `T: Copy` returns `None` unless `elements`' byte range is inside `owner.bytes()`.
     The `&[T]` is already aligned, since `Slice` checks it.
   - Keep `size_of::<Col<u32>>() == 32` if it can be kept, e.g. by making the slab a `Held`
     too. Otherwise move the assertion and say so.
2. `crates/hir-def/src/expr_store/persist.rs`: add
   `pub fn decode_in(self, owner: &crate::deps::ImageBuf) -> Result<ExpressionStore, FormatError>`
   beside `decode` (L1262). It is the same decode, with each `Copy` column made by
   `Col::borrowed(owner, slice)` instead of the slab copy. A `None` from `borrowed` is a
   `FormatError` ("a column outside its image"), never a copy.
3. Test beside `decode`'s round trips: `decode_in` of a store written into an `Aligned` held
   as an `ImageBuf` equals `decode`, and its `Copy` columns point into the buffer.

Why: principles.md's zero-copy reads of mapped bytes. A served dependency's signature stores
are read where the store maps them; a copy is about 11% over decode's bound
(`docs/v2/plan/lanes/format/speed-of-light.md`, "Encoding and decoding a store").

Status: open

## mem-db-infer-method, for lane items: `method_resolution.rs`, `method_resolution/`, `traits.rs`, `specialization.rs`, `opaques.rs`, `drop.rs` (from db-mem-infer-b, 2026-10-29)

Lane mem's allocation requests for lane items' files of hir-ty, in
`v2/requests/mem-db-infer-method.md` (exact before/after code there; this lists them, it does not
copy them). Lane db's own entries of that file (P6 `autoderef.rs`, N1 `inhabitedness.rs`, R1
`representability.rs`, H1/H2 in `crates/hir`) are db-mem-infer-b's.

- S1 (line 81): the frame map and set aliases. `v2/requests/mem-db-infer.md` section 5 defines
  them once in hir-ty's `lib.rs`; `method_resolution.rs` writes `pub(crate) use crate::{FrameMap, FrameSet};`
  instead of S1's own definitions.
- P1 (104): `MethodResolutionContext` gains `frame: &'a Frame`. db's H1 (`hir/src/attrs.rs`) and
  H2 (`hir/src/lib.rs` `Type::with_method_resolution`) fill it and wait on P1; tell db-mem-infer-b
  when P1 is in.
- P2 to P5 (172, 253, 352, 414): `probe.rs`' candidate lists, `impl_dups`, autoderef steps,
  `TraitValueItems::runs` and `ProbeAllChoice` in the frame.
- C1, C2 (673, 724): `ConfirmContext` and its lowerer carry `'f`; the receiver's adjustments
  stay in the frame (with mem-db-infer-expr X1/X2, db-mem-infer-a's).
- I1 to I6 (796 to 1356): the impl tables built once at exact size by frame builders; frozen
  reads push into the builders (`frozen.rs`); the many-crate lookups collect exactly.
- M1 to M8 (1517 to 1998): `trait_value_items`, `in_crate_trait_value_items`, `loaded_crates`,
  `macros_writing_impls`/`macro_definitions`, `impls_in_bodies`, `impl_holders`, `impl_work`
  (an entry point opening `frame::task`), tags.
- O1 to O4 (2013 to 2168): `opaques.rs`' queries in the frame, results exact.
- SP1 (2179): `constrained_params`/`parameters_for` in the frame.
- D1 (2318): `has_drop_glue`'s visited set in the frame, with `has_drop_glue_in(frame, ..)`.
- T1, T2 (2606, 2657): `where_predicate_must_hold` opens a task and passes the frame to
  `TyLoweringContext::new` (waits on mem-db-infer-lower's R2 change of `new`, db-mem-infer-b-lower's);
  tags.
- "Tracked functions" (2755), "Entry points" (2805), "R7" (2831), "Tests" (2901: `columns_tests`
  and `macro_definitions_iterate_their_runs`) for these files.
- Where two parts meet in these files: `v2/requests/mem-db-infer.md` section 6, items 4
  (`confirm.rs`), 5 (`with_method_resolution`), 8 (`crates_containing_incoherent_inherent_impls`)
  and 9 (`frozen.rs`).
- Tag form: salsa's `mem_tag` option as filed (`v2/requests/mem-db-salsa.md` M2) takes a bare
  `blaze_mem::Tag` variant, so lane db writes `mem_tag = Infer` where the request file says
  `mem_tag = crate::db::INFER_TAG`; lane items should match.
