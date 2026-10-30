# Requests of db-ty-hir

## 1. Coordinator / lane db: `RootDatabase::run_all` hands each task the database it was called on

What: `SourceDatabase::run_all(&self, count, task)` on the real database (blaze-engine's
`RootDatabase`, ide-db's) passes `self` to every `task(db, i)`, never another object (no clone,
no handle), as base-db's doc says ("each reading this database by reference").
Where: whichever crate implements `run_all` for the daemon's database (db-expand's request).
Why: `crates/hir/src/check.rs` (`check_modules`) runs `check_module` per file module as one
`run_all` batch and gives back `AnyDiagnostic<'db>` of the database it was called with. Each
task downcasts the `&dyn SourceDatabase` it is handed to `&dyn HirDatabase` and asserts it is
the same object (it panics otherwise, naming why), then keeps the diagnostics in the caller's
lifetime. The tasks also move `AnyDiagnostic<'db>` between threads, so `AnyDiagnostic` must stay
`Send` (it is today: interned types, pointers, names, strings).

> applied: `RootDatabase::run_all` (`crates/ide-db/src/lib.rs`) calls `task(self, at)` on every job: the one database by reference, no clone or handle; hir-ide's `TestDB` likewise.

## 2. Coordinator, blaze-engine: the check driver (db-ty's request 3, updated)

What: `hir::check::check_crate(db, krate, style_lints, bodies)` now takes
`bodies: Option<&(dyn Fn(DefWithBody) -> bool + Sync)>` (`hir::check::Bodies`), since its
modules run as plane tasks. A filter over a set (`|it| asked.contains(&it)`) is `Sync` already.
`check_workspace`/`check_crates` check the modules of all their crates as one batch, and
`hir::check::check_module(db, module, style_lints, bodies)` is the work of one task, for a
driver that schedules modules itself (e.g. on the plane, one node per file module from
`hir::check::file_modules`). Results are in def-map order whatever the order tasks ran in.
Where: `crates/blaze-engine/src/lib.rs` `errors_of` (~6615), in place of
`ide_diagnostics::semantic_diagnostics_of_bodies`.

> forwarded to codegen: `crates/blaze-engine/src/lib.rs` is codegen's.

## 3. db-ty (parent): `crates/hir-ty-wfcheck` gains `blaze-format` and `la-arena`

Both are workspace dependencies already (`blaze-format` is in hir-ty's manifest). Used by
`crates/hir-ty-wfcheck/src/persist.rs` (`impl Persist for WfResult`). The manifest lint passes.

> forwarded to db: `crates/hir-ty-wfcheck/Cargo.toml` is lane db's (both are workspace dependencies already).

## 4. Lane db: `AnyDiagnostic` must stay `Send + Sync`; `hir-ty-wfcheck` gains `smallvec`

`check_modules` (`crates/hir/src/check.rs`) now keeps each module's diagnostics in a
`OnceLock` slot written once by its task (a compare-and-swap, no lock), which needs
`AnyDiagnostic<'db>: Send + Sync` (it is today: interned types, pointers, names, strings).
`crates/hir-ty-wfcheck/Cargo.toml` gains `smallvec.workspace = true` (outlives components and
the region walk's stacks on the stack); the manifest lint passes.

> forwarded to db: `crates/hir/` and `crates/hir-ty-wfcheck/Cargo.toml` are lane db's (`smallvec` is a workspace dependency already).

## 5. Lane db: name the techniques in `docs/v2/plan/lanes/db/query-database.md`

The notes are lane db's file; a paragraph for its front-end section:

- The check (`hir::check`) matches rustc's parallel front end (`par_hir_for_each_module`,
  one module per rayon task) and beats it twice: a run of small modules is one plane task
  (32 items or more but for the last, cut in def-map order, so the cut is deterministic), and
  the diagnostics come out in def-map order whatever the threads did, which rustc's don't.
- Well-formedness (`hir-ty-wfcheck`) matches rustc's `check_well_formed` under
  `enter_wf_checking_ctxt`: one inference context per item, built only for an item with
  aliases to normalize; outlives components on the stack; the result a zero-copy record
  (`WfResultRef`), 24-byte rows with the text after them.

> forwarded to db: `docs/v2/plan/lanes/db/` is lane db's notes folder.

## 6. db-expand (hir-def): `DefMap::diagnostics` grouped by module

`hir::DiagnosticsCollector::collect_module` scans every diagnostic of the def map once per
module (`FIXME: This is accidentally quadratic`), and a whole-crate check now visits every
module. Keep `DefMap::diagnostics` sorted by `in_module` (or give `DefMap` a
`diagnostics_of(module) -> &[DefDiagnostic]`), and hir will take its range instead.

> forwarded to db: `crates/hir-def/src/nameres/` is lane db's (db-expand).
