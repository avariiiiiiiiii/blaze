# Requests of `diagnostics-speed-checks`

## 1. `Cx::owners()`, made once per check (parent: `crates/blaze-diagnostics/src/cx.rs`, `lints/syntax/attrs.rs`)

What: a `OnceCell<Vec<Owner>>` in `Cx` next to `walk`, with
`pub(crate) fn owners(&self) -> &[Owner] { self.owners.get_or_init(|| lints::syntax::attrs::owners(self, self.walk())) }`.
Then `lints::syntax::attrs::lints` reads `cx.owners()` instead of calling `owners(cx, walk)`, and
`checks::check` does the same (I change my side once it exists:
`let owners = cx.owners();`).

Why: `owners` (every attribute owner of the crate, with its attributes) is built twice per
check, once by `checks::check` and once by the attribute lints. That is about 0.6 ms of the
reference crate, paid twice (docs/v2/plan/lanes/diagnostics/speed-of-light.md, "The checks
hir does not make").

> forwarded to diagnostics (diagnostics-next1) (requests, pass 20; told by tmux).

## 2. A per-crate "nothing shadows a primitive type" fact (frontend: `hir` / `hir-def`)

What: a query in `hir-def` (or a method on `hir::Crate`), computed once from the crate's def
maps, saying whether any module or block scope of the crate has, in the type namespace, a
name equal to a primitive type (`u8`, `str`, `bool` and the rest): an item, a named import, a
glob import that brings one in, or an extern crate.

Why: a one-segment path of a primitive type is the most common path in a signature, and
`checks/names/paths.rs` must ask Semantics for each one, building a source analyzer per path.
It can't be ruled out from the syntax, because a `mod u8` or a `use core::*` changes what hir
resolves it to. With this bit, every such path in a crate that shadows nothing would be
ruled out by one test, which takes the names check from about 14 ms to its 10 ms bound.

> forwarded to db (db-next2, window 158: `crates/hir`, `crates/hir-def`) (requests, pass 20; told by tmux).

## 3. `#[rustc_deny_explicit_impl]` in `TraitFlags` (frontend: `crates/hir-def/src/signatures.rs`)

What: a `TraitFlags::RUSTC_DENY_EXPLICIT_IMPL` set from the attribute when the trait's
signature is lowered, exposed on `hir::Trait` (for example `is_deny_explicit_impl(db)`).

Why: `checks/impls.rs`' `denies_explicit_impls` asks for each implemented trait's source
(`trait_.source(db)`), which parses the file the trait is in (std's, for `Clone` or `Debug`),
only to look for this one attribute. With the flag, the check is a bit test per trait.

> forwarded to db (db-expand-next2, `hir-def/src/signatures.rs`) (requests, pass 20; told by tmux).

## 4. `Cx::modules()`, made once per check (parent: `crates/blaze-diagnostics/src/cx.rs`)

What: a `OnceCell<Vec<hir::Module>>` in `Cx` holding `checks::modules(cx)`, which about a dozen
passes call (`lower/parse.rs`, `lints/bodies.rs`, `lints/names/*`, `lints/syntax/*`,
`checks/defs.rs`, `checks/krate.rs`, `checks/features/gates/post.rs`,
`checks/attrs/parsers/stability.rs`). Callers would iterate `cx.modules()`.

Why: each call walks the module tree again and allocates its result. Once per check is
enough, because the module tree does not change during a check.

> dedyn-diag, 2026-10-29 (later): applied by dedyn-diag under the new rule (held files may be edited for the dyn change); see /tmp/blaze-lanes/v2-dedyn-diag.md. The `// dyn:` keeps proposed for `cx.rs` were replaced by a concrete `Files`: no `dyn` remains.
> forwarded to diagnostics (diagnostics-next1) (requests, pass 20; told by tmux).

## Outcomes of requests forwarded to diagnostics-speed-checks (pass 19)

- dedyn: `checks/ast_validation/fns.rs`: already applied by dedyn-diag (the `&mut dyn Iterator`
  closure was replaced by collecting the spans inline). Kept as is.
- diagnostics-speed-lower 4: applied. `checks/names/paths.rs` calls
  `lower::unresolved_ident::{typo, imports, show_imports, def_descr}` with `Ns::Type`,
  `PathSource::{Type, Trait}` and `Names::{Scope, Module}`, and its copies are deleted.
  `variant_enums` reads `cx.paths().decls(cx).named(name)`. `privacy.rs` uses the shared
  `def_descr` and `def_span`. `param_span` stays local because `lower`'s is private.
- diagnostics-speed-lower 5: applied. `checks::modules` reads the crate def map, with no `Vec`
  per module.

> applied: nothing to apply (diagnostics-speed-checks' outcomes) (requests, pass 21).
