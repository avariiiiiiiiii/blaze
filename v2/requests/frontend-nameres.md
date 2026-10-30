# Requests of frontend-nameres

Index of every request the `frontend-nameres` children send (owner hooks live in the owner's
file under `## frontend-nameres-<part>:` headings; coordinator requests are written here in
full). Append only, under your part's heading.

## parent

- Coordinator: when `frontend-nameres-core`'s db hook lands, `crates/hir-def/src/nameres/facts/`
  becomes compiled (`mod facts;`); before that, `facts/ops.rs` (committed) is not part of the
  build. Commit the `facts/` tree, `docs/v2/plan/lanes/frontend/nameres*.md` and the hooks
  together.

> forwarded to the coordinator: committing is its; listed in requests' report.

## core

## graph

- Coordinator (`v2/requests/frontend.md`, `## frontend-nameres-graph: proptest as a workspace
  dependency`): `proptest = "1.5"` in the root `[workspace.dependencies]`. Open.
- db (`v2/requests/db.md`, `## frontend-nameres-graph: proptest for hir-def's tests`):
  `proptest.workspace = true` in hir-def's `[dev-dependencies]`; also relies on speed-hir-store's
  counting-allocator hook (items 2-4) for `blaze_alloc_count::measure`. Open.

## impls

## diff

## oracle
