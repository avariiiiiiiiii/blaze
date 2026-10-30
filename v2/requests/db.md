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

`docs/v2-db.md` moved to `docs/v2/plan/lanes/db/query-database.md` (lane notes go in the lane's
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

Status: open

## speed-hir-store: hir-ty-wfcheck onto the HIR column views (from speed-hir-store-ty)

1 site in 1 file, a type ref (rule: addendum of `~/restart-2026-10-28/briefs/v2/speed-hir-store-consumers.md`, landed: `Index<TypeRefId>` is gone).

> forwarded to db.

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

### hir-expand

- `crates/hir-expand/src/lib.rs:8` crate: no `unsafe` in its compiled code. The `unsafe` in `builtin/attr_macro.rs:111-154` is token text inside `quote::quote!` (the code `#[global_allocator]` expands to in the user's crate), not code of hir-expand; `lib.rs:1578` is the salsa `unsafe(no_lifetime)` option. Add `#![forbid(unsafe_code)]` after `#![allow(rustdoc::private_intra_doc_links)]`.

### hir

- `crates/hir/src/lib.rs:634` block (`Module::nearest_non_block_module`, `unsafe { id.to_static() }`): no SAFETY. The block shares its line with `Module { id: ... }`, so the comment goes on the line above it:
  ```rust
  // SAFETY: `to_static` needs the id not to outlive the query computation it was made in. `id` is `self.id` or a module containing it, read from `db`'s def maps, and `Module` holds it as `'static` as every `hir` handle does, used only with the database it came from. A `Module` kept past the revision that removed its module would name a freed salsa slot.
  ```
- `crates/hir/src/check.rs:128`, `:162`: ok.

### hir-def (db's files)

The `to_static`/`to_db` calls all follow the house style `expr_store/lower.rs:2808` and `:3085` already use.

- `crates/hir-def/src/lib.rs:657` block (`BlockIdLt::to_static`, a `transmute`): no SAFETY. Proposed: `` // SAFETY: `BlockIdLt<'db>` and `BlockId` differ only in a lifetime, so they have one layout and the bits are kept. The lifetime is what the caller answers for (`# Safety`): the id must not outlive the query computation it was made in, or it may name a freed salsa slot. `` Its `# Safety` also says "the `ModuleId`" where it means "the `BlockId`".
- `crates/hir-def/src/lib.rs:665` block (`BlockId::to_db`): no SAFETY. Proposed: `` // SAFETY: the same type but for the lifetime, as in `to_static`. The caller makes `self` an id of `_db` (`# Safety`), so `'db`, the borrow of `_db`, is its lifetime. An id of another database would name a slot of another table. ``
- `crates/hir-def/src/lib.rs:694` block (`ModuleIdLt::to_static`): no SAFETY. Proposed: as for `lib.rs:657` with `ModuleIdLt<'db>`/`ModuleId`.
- `crates/hir-def/src/lib.rs:747` block (`ModuleId::to_db`): no SAFETY. Proposed: as for `lib.rs:665` with `ModuleId`.
- `crates/hir-def/src/find_path.rs:311`, `:328`, `:2616`, `:2634` blocks (`module_id.to_static()` into an `ItemInNs`): no SAFETY. Proposed, on the line above each block's line: `` // SAFETY: `to_static` needs the id not to outlive the query computation it was made in. The `'static` id only keys this lookup (`find_in_scope`/`find_in_prelude` compare it with the def map's items) and is dropped before `find_path` returns; the `Choice` holds names, not ids. If it were stored, a later revision could read a freed salsa slot through it. ``
- `crates/hir-def/src/nameres.rs:552` block (`block_def_map`, `block_id.to_static()`): no SAFETY. Proposed: `` // SAFETY: `to_static` needs the id not to outlive the query computation it was made in. `block_id` is this query's own key, and the `'static` copy goes only into the `DefMap` this query returns, which salsa invalidates with the key. Stored anywhere else, it could name a freed salsa slot in a later revision. ``
- `crates/hir-def/src/nameres.rs:554` block (`block_id.module(db).to_static()`): no SAFETY. Proposed: `` // SAFETY: as for `block_id` above: the block's module goes only into the `DefMap` this query returns. ``
- `crates/hir-def/src/nameres.rs:593` block (`DefMap::empty`, `ModuleIdLt::new(..).to_static()`): no SAFETY. Proposed: `` // SAFETY: `to_static` needs the id not to outlive the query computation it was made in. `empty` runs only inside the queries that build a def map (`crate_def_map`, `block_def_map`, and `frozen`'s decode within them), and the root id goes only into the `DefMap` that query returns. Stored elsewhere, it could name a freed salsa slot in a later revision. ``
- `crates/hir-def/src/nameres.rs:661` block (`DefMap::module`, `id.to_static()`): no SAFETY. Proposed: `` // SAFETY: `to_static` needs the id not to outlive the query computation it was made in. The `'static` copy only keys the lookup in `self.modules` and is dropped when this returns; nothing stores it. ``
- `crates/hir-def/src/nameres.rs:1288` block (`Index for ModulesMap`, `id.to_static()`): no SAFETY. Proposed: on the line above `.get(...)`, `` // SAFETY: as in `DefMap::module`: the `'static` copy only keys this lookup and is dropped when it returns. ``
- `crates/hir-def/src/visibility.rs:63` block (`Visibility::is_visible_from`, `m.to_db(db)`): no SAFETY. Proposed: `` // SAFETY: `to_db` needs `m` to be an id of `db`. A `Visibility` is resolved against `db`'s def maps (`Visibility::resolve`, the collector) and asked only with the same database, so `m` is one of `db`'s modules. A module of another database would name a slot of another table. ``
- `crates/hir-def/src/attrs/docs.rs:367` block (`as_mut_vec().clear()` in the guard's `drop`): no SAFETY. Proposed: `` // SAFETY: `as_mut_vec` needs the bytes to be UTF-8 again when the borrow ends; `clear` leaves none. The guard runs only when `remove_indent` unwound partway through, when the bytes may not be UTF-8, which is why `String::clear` is not used. ``
- `crates/hir-def/src/attrs/docs.rs:437` block (`as_bytes_mut().copy_within(..)`): no SAFETY. See the MAY BE UNSOUND note below; if kept as is: `` // SAFETY: `as_bytes_mut` needs the bytes to be UTF-8 again before the `str` is used. This moves the line, without its indent, down to `copy_into`, so the bytes from there to the next line's start may split a character until the `truncate` below; on a panic the guard clears the string. ``
- `crates/hir-def/src/attrs/docs.rs:465` block (`as_mut_vec().truncate(copy_into)`): no SAFETY. Proposed: `` // SAFETY: `[0, copy_into)` is the lines copied down, each cut at a character boundary (`indent_size` is a `char_indices` offset), so what is left is UTF-8. ``
- `crates/hir-def/src/attrs/docs.rs:419-443` MAY BE UNSOUND (a library contract, not memory): between `as_bytes_mut` (`:437`) and the `truncate` (`:465`), the loop keeps using `guard.0.docs` as a `str` (`TextSize::of(&guard.0.docs)` and `&guard.0.docs[range]` at `:421`/`:424`) while the bytes between the copied text and the next line can be a split UTF-8 sequence. That breaks `str::as_bytes_mut`'s contract ("valid UTF-8 before the borrow ends and the underlying `str` is used"). In practice each read touches only untouched lines and boundaries, so nothing reads the bad bytes. Fix without changing behavior: take `let bytes = unsafe { guard.0.docs.as_mut_vec() }` once before the loop, slice lines from `bytes` (each untouched range is UTF-8: `str::from_utf8_unchecked` with its own SAFETY), and use `bytes.len()` for the length.
- `crates/hir-def/src/nameres/collector.rs:3249` block (`ModuleIdLt::new(..).to_static()` for a new child module): no SAFETY. Proposed: `` // SAFETY: `to_static` needs the id not to outlive the query computation it was made in. The collector runs inside the def-map query (`crate_def_map`/`block_def_map`), and the id goes only into the `DefMap` it builds (`modules`, the parent's children, the scope), which salsa invalidates with that query. Stored elsewhere, it could name a freed salsa slot in a later revision. ``
- `crates/hir-def/src/nameres/demand.rs:209`, `:216`, `:224` blocks (`block_owner`, `to_static` of a module and an outer block): no SAFETY. Proposed, above `:209`: `` // SAFETY: `to_static` needs the id not to outlive the query computation it was made in. `module` and `outer` are locals of this walk, only asked for their def maps and blocks with `db`, and dropped before `block_owner` returns a `DefWithBodyId`; nothing stores them. `` Above `:216` (inside the match arm, the line before it) and `:224`: `` // SAFETY: as for `module` above: a local of this walk, never stored. ``
- `crates/hir-def/src/nameres/frozen.rs:996` block (`block.to_static()` while encoding a module): no SAFETY. Proposed: `` // SAFETY: `to_static` needs the id not to outlive the query computation it was made in. `block` only finds its owner (`block_owner`), whose path is what gets written, and is dropped when this returns. ``
- `crates/hir-def/src/nameres/frozen.rs:2922` block (`ModuleIdLt::new(..).to_static()` while decoding a def map): no SAFETY. Proposed, above `let module =`: `` // SAFETY: `to_static` needs the id not to outlive the query computation it was made in. The decode runs inside the def-map query that reads the frozen def map, and the ids go only into the `DefMap` it returns (`self.own`, then its modules), which salsa invalidates with that query. ``
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

This is the only caller (`rg get_traits_in_scope crates/`). Status: open

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
