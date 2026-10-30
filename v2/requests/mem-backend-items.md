## mem-backend-items: `Cargo.lock` for blaze-items's new dependencies

From `mem-backend-items` (report `/tmp/blaze-lanes/v2-mem-backend-items.md`, design
`docs/v2/plan/lanes/mem/backend-items.md`). `crates/blaze-items/Cargo.toml` now depends on
`blaze-mem` and, for tests, `blaze-alloc-count`; both are workspace dependencies already. The
lane may not run cargo, so `Cargo.lock` is not updated; its `blaze-items` entry already lacks most
of the crate's dependencies (it lists `blaze-format`, `blaze-ids`, `blaze-query`, `serde`), so the
next regeneration picks these up with them.

Status: open, for whoever may run cargo.

> applied: on the requests report's "lock additions" list (blaze-items gains `blaze-mem` and dev `blaze-alloc-count`, both workspace members), for the first compile; `Cargo.lock` left alone as the brief says.

## mem-backend-items to lane mem: frames and shelves deep enough for the front end

`blaze_mem::frame` keeps `DEPTH` (8) frames warm per thread and maps a fresh 4 GiB region for a
task nested deeper; `scratch` keeps 4 buffers a kind. In blaze-items:

- the hash queries recurse as deep as the graph of refs, so `component_hash` cannot hold a frame
  across its fills and gathers what it spells on the heap (`hash.rs`);
- form spelling nests as types do and keeps its own thread-local pool of 64 form buffers
  (`form.rs` `SPARE`) rather than `scratch`, which would allocate past 4.

Asked: either a nested task past `DEPTH` that shares its parent's frame (a mark and a release,
no new mapping), or a `DEPTH` sized for salsa's query nesting; and typed scratch shelves (a
`Vec<T>` of a `Copy` type, or a byte buffer reinterpreted) at least 64 deep. With either, both
places above move and the crate's last per-call heap lists go.

Status: open, for lane mem.

> forwarded to mem (from requests, pass 17; told by tmux).

## mem-backend-items to lane mem / db-salsa: memo values in revision arenas

Every index memo in blaze-items (`HashColumns`, `FactSet`, `Runs`, `Keyed`, `ScopeNames`, a
store's keys) is a few exact `Box<[T]>`s: the floor for a value the heap holds. A region per
memo would exhaust Linux's 65 530 mappings. A memo value allocated in an arena per revision
(freed when salsa drops the revision's memos) would make each one a bump; that is salsa's to
offer (`SalsaValue` takes an owned `'static` value today).

Status: open, for lane mem with db-salsa.

> forwarded to mem (from requests, pass 17; told by tmux).

## mem-backend-items: waiting on db for two edits to blaze-items

Taken on 2026-10-29 20:36, to apply in `crates/blaze-items` once their dependencies land:

- mem-db-def B6 (`v2/requests/mem-db-def.md`): `index.rs` `dependents` goes and `trait_impls`
  calls `trait_.krate(db).transitive_rev_deps(db)` directly. Waits for B6: the walk is unmemoized
  today, so removing `dependents` first would rebuild the inverted crate graph per trait.
- dedup-scan (`v2/requests/items.md`, `## dedup: hir_def::test_items`): `tests/narrow.rs` and
  `tests/coherence.rs` onto `hir_def::test_items`. Waits for `crates/hir-def/src/test_items.rs`
  (`v2/requests/db.md`).

Status: waiting on db; whoever lands either may apply its edit here (rule 13) and say so.

> forwarded to db (from requests, pass 17; told by tmux).

## mem-backend-items to lane items / frontend-nameres: `ScopeEntry` that borrows

`blaze_items::scope` returns an owned `ScopeEntry`, so a warm lookup of an associated name or of
a module name with `macro_rules!` in scope boxes `Assoc` or `legacy_macros` (`index.rs`
`ScopeNames::get`). An entry borrowing the `scope_names` memo (`&'db [..]`, or a run index into
it) makes every warm lookup allocation-free. It is an interface change for `frontend-nameres*`,
which read `index.rs`, so it is not made here.

Status: open, for whoever owns the `scope` interface.

> forwarded to frontend-nameres (from requests, pass 17; told by tmux).
