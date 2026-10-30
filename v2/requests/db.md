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

## frontend-text: one lazy line index per file text (frontend.md item 6; 2026-10-29)

`span::LineIndex` is new (lane `frontend-text`, `crates/span/src/lines.rs`): a bitmap of the
text's `\n` bytes made with SIMD (AVX-512BW/AVX2/SSE2/NEON, chosen at run time, every path
checked against the scalar one) and a rank table per 512 bytes; `place(offset)` gives a
`span::Place` whose `line()`, `char_column(text)`, `display_column(text)` and
`byte_column(text)` are rustc's (`lookup_char_pos`: byte order mark dropped, `\r\n` read as `\n`,
a last `\n` starting no line; tested against a model of `rustc_span::SourceFile` at every
offset). Today two salsa queries scan each text for line starts (`hir-expand`'s `line_starts`,
`hir-ty-mir`'s), plus load-cargo's `line_index` crate. One query in base-db replaces them.

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
