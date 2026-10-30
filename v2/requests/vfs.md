# Requests from the vfs lane

## 1. core: a directory listing effect (blocks the watcher's scan and rescan)

What: in `crates/blaze-core/src/lib.rs`, `Effect::List { dir: FileName, tag: Tag }` completing
with `Done::Listed(Vec<(FileName, Meta)>)`: every file and directory under `dir`, recursively,
with its `Meta`; in the simulation, threads and io_uring drivers.

Why: `blaze_vfs::Watcher` needs it (a) to load a directory into a head when a watch starts
and (b) to rescan after `Done::Changed { lost: true }`. Without it the watcher stops on `lost`
with `Watched::Lost`, and the caller must start it on a head that already holds what the
directory does. Once it exists, the vfs lane makes `lost` a rescan (diff of listing and head)
and adds `Watcher::scan`.

Status: done. Core added `Effect::List` and `Done::Listed`; the watcher scans with it when a
watch starts and rescans on `lost`. Note for core: the simulation's `List` fails for a
directory that holds no file (it has no directory entries), where a real one lists nothing.

> applied: by core and vfs (status above).
> forwarded to core: the note on the simulation's `List` of a directory with no file.

## 2. interface: `Vfs::apply` with an unknown base

What: in `crates/blaze-vfs/src/lib.rs`, `Vfs::apply` returns `Result<ContentId, CommitError>`
(or `OpError` gains `UnknownRoot(ContentId)`).

Why: `OpError` has no variant for "the base root is not one this tree has"; today it returns
`OpError::NotFound("root <64 hex digits>")`, which names it but is not typed. `blaze-api`'s
`ViewRef::WhatIf` is the caller.

> applied: `OpError::UnknownRoot(ContentId)` in `crates/blaze-vfs/src/lib.rs` (vfs is DONE), shown as "no root {root} is known" like `CommitError::UnknownBase`; `Vfs::apply` returns it for an unknown base; `tests.rs` expects it. ci's `op_path` (`crates/blaze-api/src/session.rs`) now returns a `String` and names `root <hex>` for it, as `resolve` names an unknown root.

## 3. interface (optional): `Vfs::diff` with an unknown root

What: `Vfs::diff` returns `Result<Vec<Change>, CommitError>`.

Why: it panics today when either root is not one the tree has (documented: callers pass roots
of open views or heads). An API request naming a root directly would reach that panic unless
the API opens both first.

> refused: the API can't reach the panic: `session.rs` resolves both views first (`resolve` refuses a root `Vfs::has` lacks), so a root unknown at `diff` is a broken invariant, which a panic names, not a request error.

## 4. integration: deleting `crates/vfs` and `crates/vfs-notify`

What: remove both crates and their root `Cargo.toml` entries once nothing depends on them.

Why: v2 replaces them with `blaze-vfs` and `blaze-core`'s watch, but they are still used by
`base-db` and `hir-*` (db), `ide-db`, `load-cargo`, `blaze-cargo`, `blaze-engine`, `blaze-doc`,
`blaze-wasm`, `span` and `vendor/proc-macro-srv`, which other lanes own. Each must move to
`blaze_vfs::Vfs` views (db: `Cx::file`) first; the vfs lane can't delete them before that.

> forwarded to coordinator: at integration, once nothing uses them; `requests` removes their root entries then (in the report's integration list).

## 5. Cargo.lock

`blaze-vfs` now depends on `rustc-hash` (already a workspace dependency).

> applied: no new package (`rustc-hash` is in `Cargo.lock`); the lock's `blaze-vfs` entry gains the name at the first compile.

## 6. core: a file's mode in `Meta`, and reading a symlink

What: in `crates/blaze-core/src/lib.rs`, `Meta` gains what `lstat` says of a file's kind and
mode (for example `executable: bool` and `symlink: bool`, from `S_IXUSR` and `S_IFLNK`, in
`Stat` and `List`, without following a link), and an `Effect::ReadLink { file, tag }` that
completes with the link's target bytes; in the simulation, threads and io_uring drivers.

Why: files carry a mode now (`blaze_vfs::Mode`, regular, executable or symlink, hashed into
the tree). The watcher can't see either today: `Stat` has no mode and `ReadAll` reads through
a link. So it keeps the head's mode for a file it has, writes a new file as regular, and
misses a chmod alone (documented in `crates/blaze-vfs/src/watch.rs`). With this it writes
the disk's mode, and a symlink as its target.

> forwarded to core: `Meta` has no mode and there is no `Effect::ReadLink` yet.

## 7. db: one view arena, and symlinks in source text

What: `blaze_query::Database::open` takes the `ViewId` that `blaze_vfs::Vfs::open` gave
rather than minting its own, and `blaze_api::build::Opened` drops its second field. Whatever
fills `base_db::ViewFile` for the front end reads a source file with `Vfs::follow`, not
`Vfs::read`: a symlink's own bytes are its target's path.

Why: the vfs arena is the one every crate uses (`docs/v2/plan/reference/crates.md`). rustc opens a
symlinked `.rs` file as its target, so the front end must see the target's text
(superset-of-rust).

> forwarded to db: `Database::open` still mints its own `ViewId` and `build::Opened` still has `queries` (`crates/blaze-api/src/build.rs:21`); the front end's `Vfs::follow` is db's.

## From concurrency: lock audit (2026-10-29)

Audited against `docs/v2/plan/design/concurrency.md`. `crates/vfs/` and `crates/vfs-notify/`
have no locks.

1. **`crates/blaze-vfs/src/reader.rs:39`, `:67`, `:45-65`: `Reader::nodes` is an
   `Arc<RwLock<FxHashMap<ContentId, Shared>>>`, read-locked on every `file` and `list`.** The
   whole path walk runs under the guard, and `file` also clones the file's `Arc<[u8]>`. That
   is two shared writes per read (the `RwLock` reader count and the refcount), on the read
   path every front-end file read takes. `publish` (`:72-78`) and `retain` (`:81-84`) take the
   write side, so every change and every collection also stalls all readers. This breaks "A
   read writes no shared memory" and "Tables are append-only".
   Fix: nodes are immutable and named by their content ID, which fits an append-only table
   exactly. Use an open-addressed table keyed by the `ContentId`'s bits. Publish each slot
   with one release store of a pointer to the immutable node, and grow by adding a level, as
   `blaze-ids` does. A read is then acquire loads only. Return `&[u8]`, borrowed for the
   task, instead of an `Arc` clone. `retain` unlinks entries, and the memory is freed by
   quiescent-state reclamation once every worker has passed a task boundary.

   **Done (vfs, 2026-10-29).** `crates/blaze-vfs/src/reader.rs` is now that table: open
   addressing on the content ID's bits; each place is published once by a release store of a
   pointer to an immutable node; a read is acquire loads and touches no count. Growth and
   collection fill a new level and swap it in by one release store, never rehashing in place,
   so readers never wait. The `Vfs` is the only writer (`&mut`), so writes take no lock
   either. Old levels and dropped nodes are retired, and freed only by `unsafe fn
   Vfs::reclaim` (quiescent state) or with the last handle. `Reader::file` lends `&Arc<[u8]>`
   (deref it for `&[u8]`), and `list` is a borrowed iterator. blaze-vfs has no lock left.
   Two follow-ups are outside these paths: items 8 and 9.

> applied: by lane vfs itself (`crates/blaze-vfs/src/reader.rs`, as its note says); 8 and 9 are the follow-ups.

## 8. db: hold a view's bytes as lent, not as an `Arc`

What: `blaze_query::Tree::file` returns borrowed bytes, and `InputData::File` keeps the
borrow rather than an `Arc<[u8]>`.

Why: the `impl Tree for blaze_vfs::Reader` in `crates/blaze-query/src/tree.rs` clones the
`Arc` because the trait asks for one. That is one shared count write per file per view, not
per read, since the database caches the input. The borrow is sound once `Database::open`
takes the `ViewId` that `Vfs::open` gave (item 7): the VFS never retires a node an open view
reaches, so bytes lent for a view stay valid until that view closes and a reclaim follows.
`MemTree` would keep its roots in a `blaze_ids::Sharded` (append-only, lock-free), which also
drops its `Mutex`, banned in blaze-query by concurrency.md.

> forwarded to db: `blaze_query::Tree` and `InputData` (`crates/blaze-query/`) are lane db's; it waits on item 7, also db's.

## 9. daemon (core/integration): call `Vfs::reclaim` at quiescence

What: once every worker has passed a task boundary after a `Vfs::collect` (or any change, for
grown levels), the daemon calls `unsafe { vfs.reclaim() }`.

Why: retired table levels and dropped nodes are held until then, so without the call memory
grows with every collection (nothing is unsound, it is just never freed until the `Vfs` and
every `Reader` are dropped). The contract is in `Vfs::reclaim`'s doc.

> forwarded to cargo: the daemon's loop is `crates/blaze-cargo/src/serve/daemon.rs`, lane cargo's (after a `Vfs::collect` and at the task boundary the plane reports).
