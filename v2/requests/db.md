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

## 2. vfs (or the coordinator's daemon glue): implement `blaze_query::Tree` over the VFS

`blaze_query::Tree` (new, `crates/blaze-query/src/tree.rs`) is how the database reads a view's
files: `file(root, path) -> Option<(ContentId, Arc<[u8]>)>` and
`list(root, path) -> Option<Vec<(String, bool)>>`, both pure functions of a root, called from
many worker threads. The VFS's nodes are content-addressed and immutable, so a handle that
shares them (not `&mut Vfs`) can answer both. Wire it in `Database::new(Config { tree, .. })`.

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

---
Routed or applied by the coordinator, 16:54.

## 4. Status of request 1 (db, 17:40)

`crates/blaze-query/` is lane db's crate (`docs/v2/plan/reference/crates.md`), so db applied
request 1 itself: `Query::Key: Clone + Send + Sync + Serialize + DeserializeOwned + 'static`. No
crate outside blaze-query implements `Query` yet; `blaze-units`' planned queries (`Plan` with key
`()`, `StdUnits` with `StdKey`) must derive serde on their keys.

## 5. blaze-query: `Cancelled` is the plane's, and its single flight is the plane's (db, 17:40)

`blaze_query::Cancelled` is now `pub use blaze_graph::Cancelled` (same unit struct, same name), and
the database's single flight is `blaze_graph::pump::Flights`, so a query's wait pumps and is in
the plane's one wait graph (#1301). Callers that wrote `blaze_query::Cancelled` are unchanged
(`blaze-units`, `blaze-usercode`); none imports both names. Nothing to apply.

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

## 7. Docs (coordinator)

`docs/v2-db.md` moved to `docs/v2/plan/lanes/db/query-database.md` (lane notes go in the lane's
folder); the one link to it (`crates/blaze-query/src/lib.rs`) is updated. Index it wherever
`python3 tools/docs.py check` wants lane notes listed.
