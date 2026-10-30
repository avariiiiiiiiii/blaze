
## Workspace: blaze-alloc-count and clippy's allocation lints

- What:
  - add `crates/blaze-alloc-count` as a workspace dependency
    (`blaze-alloc-count = { path = "crates/blaze-alloc-count" }`);
  - in `[workspace.lints.clippy]`, set to `"deny"`: `redundant_clone`, `clone_on_ref_ptr`,
    `implicit_clone`, `unnecessary_to_owned`, `inefficient_to_string`, `format_collect`,
    `needless_pass_by_value`, `large_types_passed_by_value`, `box_collection`, `vec_box`,
    `rc_buffer`, `large_enum_variant`.
- Where: root `Cargo.toml`.
- Why: `docs/v2/plan/goals/principles.md`, "Allocation, `dyn` and clones are controlled
  mechanically".

> applied by requests: `blaze-alloc-count = { path = \"crates/blaze-alloc-count\" }` in `[workspace.dependencies]` (new to the lock), and the twelve lints `\"deny\"` in `[workspace.lints.clippy]` under their own heading citing principles.md; `rc_buffer` moved there from the warn list.

## tools/v2/hot_lint.py crashes when given `crates` (from db-ty-port-infer-relate)

- What: `{"root": ".", "crates": ["hir-ty"]}` fails with `typed_input: crates has a type it does
  not read: list[str]`. `python/typed-input` does not read `list[str]` fields, so either it
  learns lists or `Input.crates` becomes a type it does read (a `tuple[str, ...]`, if supported).
- Where: `tools/v2/hot_lint.py` (`Input`) or `python/typed-input/src/typed_input/__init__.py`.
- Why: the brief's command doesn't run. Without `crates` it works (all hot crates).

> forwarded to the coordinator: `tools/` is its, not requests' (listed in requests' report).

## speed-hir-store-stmts: statements become columns, `Statement` a view (hir-ide)

From speed-hir-store-stmts (child of speed-hir-store), 2026-10-29. `crates/hir-ide/` is the
coordinator's (`docs/v2/plan/reference/crates.md`) and `crates/hir/` still builds on it, so it
must compile against this change. `hir-def` stores a block's statements as rows of two columns
(5 bytes a statement, a `let`'s ids in `extra`), where it was a 32-byte enum in a flat array.

**The rule.** `Statement` is now `Statement<'a>`, a `Copy` view decoded from the rows, with
today's variant and field names. `Expr::Block { statements, .. }` gives `statements:
Statements<'a>`, a `Copy` handle (`len`, `is_empty`, `get(i) -> Option<Statement<'a>>`, `first`,
`last`, `iter()` double-ended and exact size, `IntoIterator` for it and for `&` it), no longer
`&'a [Statement]`. A statement is a value: its id, `Option` and `bool` fields bind by value
(`*pat` -> `pat`, `Some(*e)` -> `Some(e)`, `match *stmt` -> `match stmt`, a `&Statement::..`
pattern drops the `&`); a parameter `&[Statement]` -> `Statements<'_>`. No slice operations.

Examples (real sites):
- `diagnostics/expr.rs` ~L321: `fn validate_block(&mut self, statements: &[Statement]) {`  ->  `fn validate_block(&mut self, statements: Statements<'_>) {`
- `diagnostics/unsafe_check.rs` ~L378: `&Statement::Let { pat, .. } => Some(pat),`  ->  `Statement::Let { pat, .. } => Some(pat),`
- `diagnostics/expr.rs` ~L500: `Statement::Expr { expr, .. } => Some(*expr),`  ->  `Statement::Expr { expr, .. } => Some(expr),`

Counts: 7 sites in 2 files of `crates/hir-ide/src/diagnostics/`. The rest of `hir-ide`
(`mir_pretty.rs`'s `StatementKind` is MIR's), `crates/hir/`, `crates/ide-db/` and
`crates/test-fixture/` have none.

### crates/hir-ide/src/diagnostics/expr.rs  (5)
- ~L47  `    hir::{Expr, ExprId, MatchArm, Pat, PatId, RecordSpread, Statement},`  ->  `    hir::{Expr, ExprId, MatchArm, Pat, PatId, RecordSpread, Statement, Statements},`
- ~L321  `    fn validate_block(&mut self, statements: &[Statement]) {`  ->  `    fn validate_block(&mut self, statements: Statements<'_>) {`
- ~L325  `            match *stmt {`  ->  `            match stmt {`
- ~L500 and ~L538 (the only two)  `Statement::Expr { expr, .. } => Some(*expr),`  ->  `Statement::Expr { expr, .. } => Some(expr),`
- unchanged: `self.validate_block(statements);` (~L159), `match statements.last()? {` (~L499, ~L537).

### crates/hir-ide/src/diagnostics/unsafe_check.rs  (2)
- ~L378 and ~L390 (the only two)  `&Statement::Let { pat, .. } => Some(pat),`  ->  `Statement::Let { pat, .. } => Some(pat),`

Status: open
