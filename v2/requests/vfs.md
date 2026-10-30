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

## 2. interface: `Vfs::apply` with an unknown base

What: in `crates/blaze-vfs/src/lib.rs`, `Vfs::apply` returns `Result<ContentId, CommitError>`
(or `OpError` gains `UnknownRoot(ContentId)`).

Why: `OpError` has no variant for "the base root is not one this tree has"; today it returns
`OpError::NotFound("root <64 hex digits>")`, which names it but is not typed. `blaze-api`'s
`ViewRef::WhatIf` is the caller.

## 3. interface (optional): `Vfs::diff` with an unknown root

What: `Vfs::diff` returns `Result<Vec<Change>, CommitError>`.

Why: it panics today when either root is not one the tree has (documented: callers pass roots
of open views or heads). An API request naming a root directly would reach that panic unless
the API opens both first.

## 4. integration: deleting `crates/vfs` and `crates/vfs-notify`

What: remove both crates and their root `Cargo.toml` entries once nothing depends on them.

Why: v2 replaces them with `blaze-vfs` and `blaze-core`'s watch, but they are still used by
`base-db` and `hir-*` (db), `ide-db`, `load-cargo`, `blaze-cargo`, `blaze-engine`, `blaze-doc`,
`blaze-wasm`, `span` and `vendor/proc-macro-srv`, which other lanes own. Each must move to
`blaze_vfs::Vfs` views (db: `Cx::file`) first; the vfs lane can't delete them before that.

## 5. Cargo.lock

`blaze-vfs` now depends on `rustc-hash` (already a workspace dependency).
