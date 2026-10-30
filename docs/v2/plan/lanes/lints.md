---
status: plan
summary: blaze-lints in v2: clippy's lints and blaze's own (`blaze::`), run per body as queries over `&Db` and the flat tree's views; hot_lint.py's rules as typed lints.
date: 2026-10-29
---
# Lane `lints`: blaze's clippy and blaze's own lints

`crates/blaze-lints` is blaze's clippy: clippy's lints, names, levels, groups, `clippy.toml` and
text, run by `blaze clippy` over the analysis `blaze check` already did. v2 moves it onto the
front end's concrete database (`base_db::Db`, [frontend/db.md](frontend/db.md)) and the flat
tree's borrowed views (`syntax::view`, [frontend/tree.md](frontend/tree.md)), runs every lint
per body as a query, and adds blaze's own typed lints in the `blaze::` tool namespace, which
replace `tools/v2/hot_lint.py`. No push for clippy parity now: a clippy lint is added only
where a v2 crate's `[lints]` sets one blaze lacks.

Design notes: [lints/cx.md](lints/cx.md) (the context every lint is written against),
[lints/blaze.md](lints/blaze.md) (the `blaze::` lints), [lints/speed-of-light.md](lints/speed-of-light.md).

## How a crate is linted

rustc's late lints read one body's HIR with its `TypeckResults` (`LateContext`); clippy's are
the same passes. v2 does that per body, as queries:

- **`body_lints(db, def, set)`** (`crates/blaze-lints/src/query.rs`): the lints of one body
  (`hir_def::DefWithBodyId`), over `hir_def::expr_store::body::body_syntax(db, def)`: the
  body's anchor parsed as a tree of its own, its ranges relative to the anchor. Edits elsewhere
  in the file move the anchor without changing that tree, so the query is not run again: only
  changed bodies are linted again. What it finds is relative to the anchor; the driver adds
  `Body::anchor_offset(db, def)`.
- **`item_lints(db, file, set)`**: the lints of what is not in a body (items, signatures, macro
  definitions, item-level macro calls), over the file's outline. An outline's bodies are lazy
  nodes, never entered here, so this reads no body and parses none.
- `set` is an interned `LintSet`: which lints may be at a level above `allow` in that body
  (the command line and the attributes of the items around the body, plus any lint the body's
  own text names) and the `clippy.toml` configuration. A body where every lint is allowed runs
  no query.
- **Types only when asked.** A lint asks for a type, a method's resolution or an adjustment
  through `hir::BodyAnalysis` (requested of lane `db`, see the request file), which infers the
  body the first time it is asked and not before. A lint that needs only syntax walks the views
  and never forces inference; a typed lint first filters on syntax (`.clone()` with no
  arguments) and asks for types only for what passes.
- **The driver** (`check.rs`, not a query) enumerates the crate's files and bodies, calls the
  queries, decides each finding's level as rustc does (the attributes around it, the command
  line, `warnings`, `#[expect]`), orders them as rustc emits them, and names files. Only the
  findings' levels are decided at the edge, so a crate with nothing found walks no attributes.

## Blaze's own lints (`blaze::`)

`hot_clone`, `hot_alloc`, `hot_dyn` and `std_hashmap` (all four the group `blaze::hot`, as
hot_lint.py checks SipHash maps in hot crates too), typed, in
`crates/blaze-lints/src/blaze/` ([lints/blaze.md](lints/blaze.md)). They are `allow` by
default, so a crate that doesn't opt in sees nothing (superset-of-rust). A crate opts in by
`[lints.blaze]` in its manifest (`hot_alloc = "deny"`), which blaze gives as
`--deny=blaze::hot_alloc` as cargo gives `[lints.clippy]`. **Hot is where the lint is on**: a
crate is declared hot by turning a `blaze::hot*` lint on for the crate (`[lints.blaze]` or
`#![deny(blaze::hot)]`), and a function is marked hot by `#[deny(blaze::hot)]` on it or on an
item around it; `#[allow(blaze::hot_alloc)]` marks a cold path inside a hot crate. This is
rustc's lint-level scoping, so no new attribute and no name that can clash with the `blaze`
crate of `#[blaze::cache]`. A line keeps the cost with a reason: `// clone: <why>`,
`// alloc: <why>` or `// dyn: <why>` on it or on the line above, as hot_lint.py has it.

`tools/v2/hot_lint.py` is deleted at integration once these build and its fixtures pass as
their tests.

`parallelizable` and `serial_dependence` (child `lints-parallel`,
[lints/parallel.md](lints/parallel.md)) are judged on MIR, in `crates/blaze-lints/src/blaze/parallel*`.
They are the same kind of `blaze::Lint` (in `Lint::ALL`, with a bit of `On` past the hot lints',
`allow` by default, opted in by `[lints.blaze]`), but in no group: `blaze::hot` stays the four
hot lints.

## Done when

- `rg 'ide_db|RootDatabase|FamousDefs' crates/blaze-lints` finds nothing; every query takes
  `&Db`; no `SyntaxNode` handle is walked (one handle per query is held to borrow its tree).
- An edit of one body lints that body again and no other (a test over `Db::for_tests` with
  salsa's event log); a crate whose lints are all syntactic runs no `infer` (the same log).
- The four `blaze::` lints, each with hot_lint.py's patterns as fixtures, and the exemption
  comments.
- The 24 clippy lints give what they gave (the existing `tests.rs` fixtures, unchanged in
  expectation).
- The workspace's `[workspace.lints.clippy]` list checked against what is implemented; every
  lint it sets that blaze lacks either implemented or listed in the report.
