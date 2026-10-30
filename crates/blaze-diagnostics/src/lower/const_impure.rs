//! A `const fn` whose result at run time depends on more than its arguments: blaze's lint
//! `blaze::const_impure`, not rustc's (docs/fn-cache.md). rustc says nothing of it, so it is
//! given only in a crate that opted into cached functions, which is one that depends on the
//! `blaze` crate (`#[blaze::cache]` calls into it, as `hir-ty-codegen/fncache.rs`
//! `uses_cache` decides); in any other crate the output is rustc's.
//!
//! Cost per diagnostic hir gives: one perfect-hash lookup of the lint and one bit test, then,
//! only where the lint can fire, one read of each direct dependency's name.

use crate::{
    cx::Cx,
    lints::{code_fact, registry},
    lower::{Found, Phase},
    model::Diag,
};

const LINT: &str = "blaze::const_impure";

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::ConstImpure) -> Vec<Found> {
    let lint = registry::lint(LINT).expect("`blaze::const_impure` is in the registry's blaze table");
    if !cx.lint_may_fire(registry::LintId::of(lint)) || !uses_cache(cx) {
        // alloc: none, an empty `Vec` allocates nothing.
        return Vec::new();
    }
    // clone: the message hir owns, once per diagnostic (the edge).
    let mut diag = Diag::lint(LINT, d.message.clone(), cx.span_of(d.node));
    code_fact(cx, &mut diag, d.node);
    // alloc: `lower`'s one `Vec` per diagnostic (lower/mod.rs's interface, the edge).
    vec![Found::lint(diag, Phase::Mir, d.node)]
}

fn uses_cache(cx: &Cx<'_, '_>) -> bool {
    cx.krate.dependencies_iter(cx.db).any(|dep| {
        dep.krate
            .display_name(cx.db)
            .is_some_and(|name| name.crate_name().symbol().as_str() == "blaze")
    })
}
