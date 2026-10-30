---
status: plan
summary: blaze v2's query database as lane db wrote it: constructive traces keyed by input hashes, a fingerprint on every edge, views as immutable roots, ordered verification, the plane's single flight, cold packs, the daemon image and the settle swap, the rerun explainer; and the front end's changes in salsa and hir-*.
date: 2026-10-29
---
# blaze v2: the query database (lane db)

Design notes for [the db lane](../db.md), as written on 2026-10-29. The code is
`crates/blaze-query/`; the front end's changes are in `vendor/salsa*`, `crates/base-db`,
`crates/hir-expand`, `crates/hir-def`, `crates/hir-ty`, `crates/hir` and the borrow checker.

## Memos: constructive traces

- A key (`QueryKey`: kind and the content ID of its arguments) keeps **every memo made of it**,
  each with its edges in the order they were read and the fingerprint each had then. There is no
  revision and no durability.
- In a view, a memo is the key's result when every edge, in order, has the fingerprint it saw.
  Verifying stops at the first that differs (#350): a key read after a changed one may not exist
  any more, and since a query is deterministic, every edge up to the first difference is one its
  execution would read too, so verifying never does work execution wouldn't.
- At most one memo of a key holds in a view (the first differing edge of two memos is the same
  key with two fingerprints), so the order memos are tried in is only a matter of speed: newest
  first.
- A memo of another producer (the code of the query's function, `Query::PRODUCER`) or value
  schema is never reused (#1439).

## Views

- A view is a root, which never changes, so what a view resolved stays resolved: each view has
  a table of key to memo, input to value, and why each key it ran did. Opening one is constant
  time; closing one retires its tables.
- Inputs are the only per-view queries: `FILE` (the file's content ID), `DIR` (the hash of its
  sorted names, not of what is under it), `CONFIG` (the bytes the view was opened with). The
  database reads them through a `Tree` (`file`, `list`), a pure function of a root that the VFS
  implements over its immutable nodes (`blaze_vfs::Reader`).
- What-if views (#276) are views of roots the VFS made without moving a head; best-of-N is N of
  them at once, sharing every memo whose inputs are equal.

## Single flight and cancellation

- One run at a time holds a key, whatever view asked. The single flight is the plane's
  (`blaze_graph::pump::Flights`, keyed by `QueryKey::task_key`), so a query's wait is one of the
  plane's waits, in the one wait graph that also holds every job pumped above a waiting task.
  The others wait, pumping, and then verify the memo it made in their own view.
- A wait that would close a cycle is refused with `Cancelled` (#1301): through other tasks'
  waits, or through a job pumped above a waiting query on its thread (a job that can't go on
  until that query does). The run that would close it unwinds, letting go of what it holds, and
  its request is reposted. `blaze_query::Cancelled` is the plane's `Cancelled`.
- A key asked again on its own run's stack is a query cycle and panics naming the chain.
- A job pumped under a waiting query is a task of its own and may ask the database. Only a
  query's own code may not: `Database::get` from inside `Query::execute` panics (ask through
  `Cx::get`, which records the edge).

## Keys by bytes

A memo's edge names a dependency by its hash only. To verify a memo whose dependency has no
valid memo in the view, the database runs the dependency by its key's bytes (deterministic CBOR),
kept beside its memos and in the image (`Query::Key: Serialize + DeserializeOwned + 'static`). A
dependency it can't run (a kind no one registered, a fact recorded with `Cx::read` that no query
answers, a key that no longer decodes) makes the memo unverifiable, never valid: the query runs
again.

## Values: memory, disk, evicted

- A value is its bytes in the zero-copy format (`Persist`), at a 16-byte boundary, read in
  place. Its default fingerprint is the first 128 bits of the hash of those bytes.
- `collect(kept, effects)` keeps what an open view or a kept key reaches (an edge reaches the
  memos of its key with the fingerprint it saw) and drops the rest. Reached values no open view
  uses are written to a cold pack (`<dir>/cold/<n>`, one atomic write), mapped, checked value by
  value against their content IDs, and then read from the mapping (#806, #734).
- `evict(key)` drops values kept elsewhere (compiled code once it is in the store): the
  fingerprint and edges stay, so cutoff still works, and a value asked for again is made again
  and must have its fingerprint, or the query is named as not deterministic.
- Nothing a reader may hold is freed while the database is shared: a buffer, memo or view let go
  of is retired, and retired things are dropped by `&mut Database` (`reclaim`, `settle`).

## The daemon image and the settle swap

- `write_image` writes every memo as one file: a header in `blaze_format::Header`'s layout with
  the image's schema hash, then memos sorted by key (192 bytes each), edges (56 bytes each), an
  open-addressed table from key to its first memo, the inputs, and a blob of values (16-byte
  aligned), key bytes and paths. The same database writes the same bytes.
- `from_image_with` checks the header and section bounds and nothing else; a memo is read the
  first time its key is asked, with its edges and value left in the mapping. A start is an mmap.
  An image of another schema is refused (`DbError::Image`) and the daemon starts empty.
- The settle swap (#1295, #1299): after a cold build the daemon writes the image; once it is
  mapped, `settle` makes the database the image's, carrying open views (each resolved memo
  matched to the image's equal one), registered queries and pending IO, and drops every arena
  of the cold build at once.

## Why did this rerun

`why(view, key)` answers for a key that ran in the view: the first edge of its newest earlier
memo that differed, then for that key the same (from its own rerun record, or by comparing the
memo with the old fingerprint against the view), back to an input, named as the view names it.
A key that was verified, not run, has no answer; one that ran for the first time has an empty
chain.

## The front end on salsa

The front end's queries stay `#[salsa::tracked]` on `vendor/salsa`, remade by the same rules:

- A memo keeps a value fingerprint through eviction, each read records the fingerprint it saw,
  and an evicted value that recomputes to an equal fingerprint is backdated, so the LRU caps no
  longer break early cutoff. No backdate when durability drops (#350).
- Interned values and symbols are never freed (`crates/intern`), so a `Hash` of their address
  is a sound fingerprint within one process. Salsa's fingerprints are not persisted.
- One `Sync` database shared by reference: no per-thread handles, no `fork`; parallel work (the
  def-map collector's expansion rounds, `SourceDatabase::run_all`) runs as plane tasks.

## Not in `blaze-query`

- Scheduling: a query runs inline on the task that asks it, and its waits pump. The plane
  (`blaze-graph`) schedules requests' roots and admits them against `admission::Estimates`;
  wiring query kinds to task kinds is the coordinator's at integration.
- The store: cold packs are the database's own files. Values the store already holds (code) are
  evicted, not packed.
