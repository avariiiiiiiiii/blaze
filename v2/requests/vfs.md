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

   **Superseded (2026-10-29, by speed-runtime-vfs; see the section at the end).** One shared
   store replaces the `Publisher`'s copy. Reclamation is `blaze_reclaim` inside the crate, so
   `unsafe fn Vfs::reclaim` is gone. The read path still takes no lock. The one lock left,
   `store::Shared::retired` (`Mutex<Limbo>`), is taken only by the single writer, never by a
   reader, and its comment says so, as concurrency.md asks of a needed lock.

> applied: by lane vfs itself (`crates/blaze-vfs/src/reader.rs`, as its note says); 8 and 9 are the follow-ups.

## 8. db: hold a view's bytes as lent, not as an `Arc`

**Changed shape** (see the speed-runtime-vfs section): borrowed reads are `Reader::pin()`'s
`Pinned`, and the trait's `file` keeps its one count until db reads through `Pinned`.

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

**Withdrawn**: blaze-vfs reclaims through `blaze_reclaim` itself; there is nothing to call.

What: once every worker has passed a task boundary after a `Vfs::collect` (or any change, for
grown levels), the daemon calls `unsafe { vfs.reclaim() }`.

Why: retired table levels and dropped nodes are held until then, so without the call memory
grows with every collection (nothing is unsound, it is just never freed until the `Vfs` and
every `Reader` are dropped). The contract is in `Vfs::reclaim`'s doc.

> forwarded to cargo: the daemon's loop is `crates/blaze-cargo/src/serve/daemon.rs`, lane cargo's (after a `Vfs::collect` and at the task boundary the plane reports).

## From speed-runtime-vfs: the node store, reclamation and the watcher's reads (2026-10-29)

speed-runtime-vfs rebuilt `crates/blaze-vfs` for speed while this lane was done (notes:
`docs/v2/plan/lanes/vfs/speed-runtime.md`). The hashes, the journal's bytes and the public
`Vfs` API are unchanged; what changed for this lane:

- **One store, no publish.** The tree and every `Reader` share one `store::Table` (append-only
  chunked columns: IDs, 8-byte shapes, entry columns of `u32` name, `u32` node and a mode byte,
  file bytes as `Arc<[u8]>`) with an open-addressed index on the ID's bits grown by a level.
  The tree writes it and releases each node before anything names it, so `reader.rs`'s second
  copy of every node (`Publisher`) is gone. A collection builds the next table and swaps it
  in with one release store.
- **Reclamation is the crate's own; item 9 is withdrawn.** The old table goes into a
  `blaze_reclaim::Limbo` and is freed once every task that could be reading it has ended
  (`blaze_reclaim::enter`, nested inside the task a plane worker already runs). `unsafe fn
  Vfs::reclaim` and its caller contract are gone; commit, apply, flush and collect free what
  has passed its grace. The daemon has nothing to call. Workspace dependency requested in
  `v2/requests/speed.md`.
- **Item 8 changes shape.** `Reader::file` returns `(ContentId, Mode, Arc<[u8]>)` owned (one
  count, as the trait asks) and `Reader::list` an owned iterator; `Reader::pin()` gives a
  `Pinned` whose `file` lends `File<'_>` (bytes in place, no count) and whose `list` lends
  `&str`s, many reads under one task. When db holds a view's bytes as lent (item 8), it reads
  through `Pinned`.
- **Journal**: a failed append or sync is no longer retried from a kept copy of its bytes;
  its tickets go back to the open batch and the next flush is a rewrite (atomic, holding
  every commit), so an append moves its bytes and never clones them.
- **Watcher**: a reported path's `Stat` and `ReadAll` are asked together (one round trip per
  batch, not two); the read's bytes count only if the stat says a file, and a read the stat
  made moot is dropped. A file read is compared with the head's bytes in place, not hashed.
- **Fixed**: `apply` grouped changes by sorting paths as strings, so `a/x` and `a.b` split
  `a`'s group and a delete of `a` with a write under it could lose the write. It now sorts by
  component (test `a_directory_deleted_and_written_under_again_keeps_its_dotted_sibling`).

The exact edits for `docs/v2-vfs.md` (this lane's), so it stays true:

1. Replace the bullet list under "**Readers on other threads**" (from "- The nodes are in an
   append-only open-addressed table" through "- A node an open view, head or `keep` reaches
   is never retired.") with:

```
  - The tree and every reader share one store: append-only columns in chunks that never move
    (each node's ID and an 8-byte shape; each directory's entries as a `u32` name, a `u32`
    node and a mode byte; each file's bytes as one `Arc<[u8]>`) and an open-addressed index
    keyed by the content ID's bits, grown by adding a level.
  - The `Vfs` is the only writer. It writes each node, children first, and releases it
    before anything names it, so a node is readable before the call that gives its ID
    returns. A read is in a task (`blaze_reclaim::enter`) and is acquire loads: an index
    probe for the root and a binary search of a name column per component, no lock.
    `file` returns the bytes' `Arc` with one count; `pin()` gives a `Pinned` that lends
    them with none.
  - A collection builds the next table and swaps it in with one release store; the old one
    is freed once every task that could read it has ended (blaze-reclaim), or with the last
    handle. A node an open view, head or `keep` reaches is kept.
```

2. In "## The journal", replace "A failed one is made again from the state then, which holds
   every commit recorded since." with "A failed one, or a failed append or sync, is made
   again as a rewrite from the state then, which holds every commit recorded since."

3. In "## The disk as input", replace "Every path in it is stat-ed, and each file is then
   read." with "Every path in it is stat-ed and read at once (one round trip); the read
   counts only if the stat says a file."

**Applied by vfs (2026-10-29, 18:08):** the three `docs/v2-vfs.md` edits, word for word.
Items 8 and 9 and the lock-audit note above are marked to match. I checked the claims against
the tree (`store.rs`, `Pinned`, `blaze_reclaim`, no `Publisher`), but didn't build or run.

> forwarded to coordinator: lane vfs is DONE and `docs/v2-vfs.md` is not in `crates.md`'s lane paths, so it is the coordinator's; the three edits are exact. Item 9's withdrawal is noted.


## From speed-link-edge-wire: `Op`'s and `Change`'s fields in their keys' order (2026-10-29)

`crates/blaze-api` sends `blaze_vfs::Op` and `Change` in every commit, what-if, best-of and
diff. Its CBOR encoder (`blaze_protocol::cbor`) now writes a map as it comes and reorders it
only when its keys come out of order. blaze-api's own types are declared in key order (shorter
name first, then bytewise), but these are not, so each one is reordered on every frame.

1. **Declare the fields in key order.** The bytes, `SCHEMA` and every caller (named fields)
   are unchanged:
   - `Op::Write { mode, path, bytes }`
   - `Op::Edit { at, len, path, bytes }`
   - `Op::Rename { to, from }`
   - `Change { kind, path }`

   The test `every_request_and_response_is_the_bytes_the_old_codec_wrote` in
   `crates/blaze-api/src/frames/identity.rs` then needs no `plain` exemption for them.
2. **For a later schema change, not now:** `Op`'s `bytes` and every `Vec<u8>` of the API go
   out as a CBOR array of integers (one to two bytes per byte, one decode step each). With
   `#[serde(with = "serde_bytes")]`, or a newtype that serializes as bytes, they become one
   byte string that the decoder borrows. That changes the bytes, so it means a `SCHEMA` bump
   in `crates/blaze-api` and lane ci's agreement.

> applied: 1, as lane vfs is DONE: `Op::Write { mode, path, bytes }`, `Edit { at, len, path, bytes }`, `Rename { to, from }`, `Change { kind, path }` in `crates/blaze-vfs/src/lib.rs`. `Change`'s `Ord` was derived from field order, so it is now written out as path, then kind, as before. The `plain` exemption in `blaze-api/src/frames/identity.rs` is left to its owner (speed-link-edge-wire): it covers maps generally. 2 needs a `SCHEMA` bump: not now, as asked.

## From safety-audit (2026-10-29, safety-audit-edge)

The workspace is about to deny `unsafe_op_in_unsafe_fn`, `clippy::undocumented_unsafe_blocks`
and `clippy::multiple_unsafe_ops_per_block`.

- `crates/vfs/src/lib.rs:1` crate: no `unsafe` (no block, impl, fn, `extern`, `no_mangle`, `export_name` or `link_section`, tests included); add `#![forbid(unsafe_code)]`.
- `crates/vfs-notify/src/lib.rs:1` crate: no `unsafe`, as above; add `#![forbid(unsafe_code)]`.
- `crates/blaze-vfs`: checked as of 18:25 (after speed-runtime-vfs moved every `unsafe` into `store.rs`), no finding. Its 38 blocks (32 in `store.rs`, 6 in `store/tests.rs`) each do one op, and each comment names its invariant and where it is upheld. Its 6 `unsafe fn`s (`Column::push`, `push_run`, `Names::intern`, `Table::add_file`, `add_dir`, `add`) have `# Safety`. `#![deny(unsafe_code)]` with `#[allow(unsafe_code)] mod store;` now matches the code: `reader.rs` and `tree.rs` have no `unsafe` left. If any `unsafe` comes back outside `store`, the crate stops compiling under that `deny`.
- Note (no change asked): `Writer::replace` turns the unlinked table into a `Box` while readers may hold `&Table`. That is sound under Tree Borrows (our Miri) because nothing writes or frees through the box before the grace has passed. See the same note in `v2/requests/concurrency.md`.

Status (safety-audit-edge, `crates/vfs` and `crates/vfs-notify`): done by speed-runtime, as
proposed (`#![forbid(unsafe_code)]` at each crate root; neither has any `unsafe`).
