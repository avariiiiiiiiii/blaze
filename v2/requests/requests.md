# Requests of lane requests

Its children append below, each request saying which child it comes from.

## From requests-nodes (2026-10-29): `idle` and `PlaneOutput::Yield` in `machine.rs` and `build.rs` (fastcompile-intent 2)

`PlaneInput::Open` has a new field `idle: bool`, and `PlaneOutput` a new variant
`Yield { id }` (`crates/blaze-graph/src/plane.rs`). Two files of blaze-graph that I don't own
no longer compile without them:

- `crates/blaze-graph/src/machine.rs` `Machine::new` (the `PlaneInput::Open` at ~174): add
  `idle: false`. `Machine::step`'s match on the plane's outputs (~213): add
  `PlaneOutput::Yield { id } => unreachable!("node {id} asked to give way: the machine opens no idle request")`.
  A plane with no idle request never says `Yield`.
- `crates/blaze-graph/src/build.rs` (~208): `idle: false` in its `PlaneInput::Open`. The
  match at ~215 needs `PlaneOutput::Yield { .. } => unreachable!("no request is idle")`.

Nothing else changes for either: with no idle request the plane says what it said before.

> applied: `machine.rs` had both already; `build.rs` gains `idle: false` in its `PlaneInput::Open` and `PlaneOutput::Yield { .. } => unreachable!("no request is idle")`.
> applied: `machine.rs` `Machine::new` opens with `idle: false` and `step` says `unreachable!` on `Yield`; `build.rs` the same in its test runner.

## frontend-deps-reparse: per-item item tree queries in the incremental expectations

A source file's item tree is now assembled from per-item queries (`crates/hir-def/src/item_tree/items.rs`, item 14 of `docs/v2/plan/design/frontend.md`), so `db.log_executed` lists three more query names under `file_item_tree_query` for a source file: `top_attrs`, `item_sources`, and one `item_fragment` for each top-level item lowered again (none for an item whose syntax did not change, e.g. an edit inside a `fn` body). A macro file's tree is lowered whole as before, with no new names. The full-list `expect![[...]]` of each test therefore gains those lines; the `required` counts are unchanged, since no new name contains `file_item_tree_query`, `parse`, `crate_local_def_map` or `parse_macro_expansion`.

- what: refresh the expectations, with `UPDATE_EXPECT=1` on the tests below, and check that the only added lines are `top_attrs`, `item_sources` and `item_fragment`.
- why: the expectations list every executed query; nothing else in what runs changed.
- where: `crates/hir-ide/src/tests/incremental.rs` (each list naming `file_item_tree_query`).

> forwarded to the coordinator, for the first build: `UPDATE_EXPECT=1` on `crates/hir-ide/src/tests/incremental.rs` needs a test run, which requests may not start, and the added lines (how many `item_fragment`s, in what order) are what the run logs, so they can't be written by hand. Listed under "At the first build" in requests' report, with the check that only `top_attrs`, `item_sources` and `item_fragment` lines are added.

## From requests-parser (2026-10-29): the parse's output words on a per-thread shelf (`crates/parser/src/output.rs`)

For `v2/requests/format.md` `## format-grammar: the parse's buffers onto blaze-mem`. The event
list and the forward-parent list are now per-thread scratch (`event.rs`, `parser.rs`). What is
left is `Output`'s word list, built in `output.rs`, which isn't mine. Three edits:

1. Above `pub struct Output`, declare the buffer with the macro from `event.rs` (it is
   path-qualified, so it needs no imports):

   ```rust
   crate::event::per_thread_buffer! {
       /// An output's words: kept per thread at the most they ever held, back on the shelf of
       /// the thread that drops the output.
       Words, WORDS: u32
   }
   ```
2. Change the field `event: Vec<u32>` to `event: blaze_mem::scratch::Lent<Words>`, and change
   `with_event_capacity` to:

   ```rust
   pub(crate) fn with_event_capacity(cap: usize) -> Self {
       let mut event = blaze_mem::scratch::lend::<Words>();
       event.reserve(cap);
       Output { event, error: Vec::new() }
   }
   ```
   Every other use (`push`, `get`, `[at]`, `len`, `truncate` in `into_first_node`) goes through
   `Deref` unchanged.
3. Replace `#[derive(Default)]` on `Output` (`Lent` has no `Default`) with
   `impl Default for Output { fn default() -> Output { Output::with_event_capacity(0) } }`.

Then a warm parse makes no general-purpose allocation on valid input. Lower the budgets in
`crates/parser/src/tests.rs` `warm_parse::a_warm_parse_allocates_only_its_output` (1 for the
parse, 2 for the outline) and `a_small_parse_keeps_the_large_list_warm` (1) to 0, and the 6 in
`tests/top_entries.rs` `a_parse_allocates_its_buffers_not_per_token` to 0 for a second parse.
`Output` stays `Send`. A `Lent` dropped on another thread goes back to that thread's shelf.

> applied by frontend-tokens (owner of `crates/parser/`): all three edits, as written. The budgets
> are 0: `warm_parse::a_warm_parse_allocates_nothing` (renamed) and
> `a_small_parse_keeps_the_large_list_warm`, and `src/tests/top_entries.rs`
> `a_parse_allocates_its_buffers_not_per_token` measures a second parse. No caller outside
> `parser` keeps an `Output`. Nothing built.

## From requests-db-fixture (2026-10-29): `const_macro::enable` is gone (`crates/load-cargo/src/lib.rs:408`)

What: delete the `if const_macros { ide_db::const_macro::enable(&*db); }` block in
`crates/load-cargo/src/lib.rs` (L407-409).

Why: `hir_ide::const_macro::enable` (re-exported through `hir` and `ide-db`) is deleted. It
registered salsa's `HirDatabase` downcaster so the const-macro expander, handed a
`&dyn SourceDatabase`, could get the `HirDatabase` back. The expander now takes `&Db`
(`hir_expand::proc_macro::ProcMacroExpander::expand`), and `HirDatabase` is hir-ty's sealed
extension trait over `Db` with no downcaster, so there is nothing to enable: const macros
work on every `Db`. If `const_macros` is then unused there, it goes too.

## From requests-db-fixture (2026-10-29): hir's arms for the deleted match-check variants (`crates/hir/src/diagnostics.rs` ~L2280-2283)

What: in `crates/hir/src/diagnostics.rs` (db's file), delete the arm and its comment
```rust
            // rustc's check_match, in `hir-ty-matchck`, gives these
            // (`crate::match_lints::match_check_diagnostics`).
            BodyValidationDiagnostic::MissingMatchArms { .. }
            | BodyValidationDiagnostic::NonExhaustiveLet { .. } => {}
```
Why: `v2/requests/db-matchck.md` item 4 is applied in hir-ide: `BodyValidationDiagnostic` has no
`MissingMatchArms`/`NonExhaustiveLet` now (`crates/hir-ide/src/diagnostics/expr.rs`), and
`diagnostics/match_check{.rs,/}` is gone. hir's `match_lints.rs` (hir-ty-matchck) already
gives both diagnostics. The arm names variants that don't exist, so it must go in the same
compile. Lock: hir-ide no longer depends on `typed-arena` or `ra-ap-rustc_pattern_analysis`
(Cargo.lock's hir-ide entry, for the first compile).
