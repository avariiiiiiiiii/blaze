//! The errors rustc gives of items, names, attributes and impls that the front end reports
//! nothing of: each module a set of rustc's checks, computed from `hir`'s public queries,
//! each `pub(crate) fn check(cx) -> Vec<Found>` in the order rustc gives them within its pass.
//! A check that needs inference or trait solving is not made here: it is the front end's, and
//! asked of it (`v2/requests/diagnostics.md`).

mod attrs;
mod defs;
mod features;
mod impls;
mod krate;
mod names;

use crate::{cx::Cx, lower::Found};

/// What rustc says of `cx.krate` that `hir` does not.
pub(crate) fn check(cx: &Cx<'_, '_>) -> Vec<Found> {
    let mut found = Vec::new();
    found.extend(features::check(cx));
    found.extend(attrs::check(cx));
    found.extend(defs::check(cx));
    found.extend(names::check(cx));
    found.extend(impls::check(cx));
    found.extend(krate::check(cx));
    found
}

/// The modules of `cx.krate` (not those of blocks), in the order rustc visits them: the crate
/// root, then each module before the modules in it, in the order they are declared.
pub(crate) fn modules(cx: &Cx<'_, '_>) -> Vec<hir::Module> {
    let mut order = Vec::new();
    let mut stack = vec![cx.krate.root_module(cx.db)];
    while let Some(module) = stack.pop() {
        order.push(module);
        let children: Vec<hir::Module> = module.children(cx.db).collect();
        stack.extend(children.into_iter().rev());
    }
    order
}
