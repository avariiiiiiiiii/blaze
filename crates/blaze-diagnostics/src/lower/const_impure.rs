//! A `const fn` whose result at run time depends on more than its arguments: blaze's lint
//! `blaze::const_impure`, not rustc's (docs/fn-cache.md). rustc says nothing of it, so it is
//! given only in a crate that opted into cached functions, which is one that depends on the
//! `blaze` crate (`#[blaze::cache]` calls into it, as `hir-ty-codegen/fncache.rs`
//! `uses_cache` decides); in any other crate the output is rustc's.

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::ConstImpure) -> Vec<Found> {
    if !uses_cache(cx) {
        return Vec::new();
    }
    let mut diag = Diag::lint("blaze::const_impure", d.message.clone(), cx.span_of(d.node));
    code_fact(cx, &mut diag, d.node);
    vec![Found::lint(diag, Phase::Mir, d.node)]
}

fn uses_cache(cx: &Cx<'_, '_>) -> bool {
    cx.krate.dependencies(cx.db).iter().any(|dep| {
        dep.krate
            .display_name(cx.db)
            .is_some_and(|name| name.crate_name().symbol().as_str() == "blaze")
    })
}
