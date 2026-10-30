# Requests of diagnostics-items

## 1. `crate::lints::syntax` reachable from `crate::checks` (parent `diagnostics`: `crates/blaze-diagnostics/src/lints.rs`)

What: `mod syntax;` → `pub(crate) mod syntax;` in `crates/blaze-diagnostics/src/lints.rs`
(`lints/syntax.rs` already has `pub(crate) mod attrs;`, `attrs.rs` `pub(crate) mod features;`).
Why: `checks.rs` builds the crate's `lints::syntax::Walk` and `attrs::owners` once and hands
them to `checks/{ast_validation,features,attrs}.rs`, which give the errors of the same rustc
passes whose lints the syntax siblings give, from the same walk and tables (one feature table,
one attribute table). Also asked by `diagnostics-lints-syntax-attrs-features` in
`v2/requests/diagnostics.md`. Without it `checks.rs` does not compile.

> forwarded to diagnostics: the parent's files.

## 2. Finer phases (parent `diagnostics`: `crates/blaze-diagnostics/src/lower/mod.rs`, `check.rs`)

What: split `Phase::Resolve` into `Expand` (expansion: macro errors, E0583, unresolved macro
calls), `Validate` (AST validation, `checks/ast_validation.rs`) and `Resolve` (the resolver's
errors at the end of `resolve_crate`, the feature gates, the early lints); and a `Lower` phase
between `Resolve` and `Items` for what attribute parsing gives as the AST is lowered
(`checks::attrs::parsed`, the lints sibling's `attrs::parsed`).
Why: rustc gives AST validation's errors after expansion's and before the resolver's
(`compiler/rustc_interface/src/passes.rs`: `configure_and_expand` runs
`ast_validation::check_crate` before `resolve_crate`), and attribute parsing's before every check
of `rustc_hir_analysis`. Today hir's `Phase::Resolve` diagnostics (expansion and resolution
alike) all come before every check's, and hir's `Phase::Items` ones before attribute parsing's,
so a crate with both kinds lists them in a different order from rustc's. Until then the checks
use `Phase::Resolve` and `checks::PASSES`.

> forwarded to diagnostics: the parent's files.

## 3. The follow-set errors of `macro_rules!` matchers (`diagnostics-lints-syntax-early`: `crates/blaze-diagnostics/src/lints/syntax/early/macros/matchers/follow.rs`)

What: give rustc's hard errors of `check_matcher_core` (`compiler/rustc_expand/src/mbe/
macro_rules.rs`): "`$x:expr` is followed by `y`, which is not allowed for `expr` fragments" with
its label and "allowed there are: ..." note, the `$x:pat` / `|` note, and the rest, as `Found`s at
`Phase::Resolve`, either from that module or as a `pub(crate)` function of a definition that
`crate::checks::macros` (mine) calls for each `macro_rules!` with `lower::macro_def_error::check`.
Why: `follow.rs` already ports FIRST/FOLLOW and `check_matcher_core`, but only for
`rust_2021_incompatible_or_patterns`; mbe checks no follow set, and `macro_def_error` does not
port it, so a matcher rustc refuses is accepted by blaze (superset-of-rust: accepting what rustc
rejects is a bug). One port of the sets, not two.
