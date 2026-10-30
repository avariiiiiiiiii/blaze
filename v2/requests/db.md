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
