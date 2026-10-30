//! The errors rustc gives of items, names, attributes and impls that the front end reports
//! nothing of: each module a set of rustc's checks, computed from `hir`'s public queries,
//! each `pub(crate) fn check(cx) -> Vec<Found>` in the order rustc gives them within its pass.
//! A check that needs inference or trait solving is not made here: it is the front end's, and
//! asked of it (`v2/requests/diagnostics.md`).

mod ast_validation;
mod attrs;
mod defs;
mod features;
pub(crate) mod impls;
mod krate;
mod macros;
mod names;
#[cfg(test)]
mod tests;

use std::ops::ControlFlow;

use syntax::ast::{self, HasAttrs};

use crate::{
    cx::Cx,
    lints::syntax::attrs::{Owner, owners},
    lower::{Found, Phase},
};

/// rustc's `misc_checking_1` (the entry point, `check_mod_attrs`): after the resolver and the
/// early lints, before the checks of items. Every check of that pass is given at this phase.
pub(crate) const PASSES: Phase = Phase::Items;

/// What rustc says of `cx.krate` that `hir` does not, in rustc's order: `#![feature]` as
/// expansion first reads it, the definitions of `macro_rules!` and the gates of macro calls'
/// attributes (as expansion meets them), AST validation, the resolver's, the feature gates
/// (with the early lints), what attribute parsing says as the AST is lowered,
/// `misc_checking_1` (the entry point, `check_mod_attrs`, the unused and stable features), the
/// checks of items.
///
/// What several checks read is made once here: the owners of attributes, the features and the
/// attribute parsers' `Session`, and the lowering of every attribute, which attribute parsing
/// and `check_mod_attrs` both read. The walk is the check's one (`Cx::walk`), which the early
/// lints and the lint levels read too.
pub(crate) fn check(cx: &Cx<'_, '_>) -> Vec<Found> {
    let walk = cx.walk();
    let owners: Vec<Owner> = owners(cx, walk);
    let flags = features::Flags::default();
    let (features, mut found) = features::configured(cx, &owners, &flags);
    let session = attrs::Session::of(cx, &owners, features);
    let features = &session.features;
    found.extend(macros::check(cx, walk));
    found.extend(features::expansion(cx, walk, &owners, features));
    found.extend(ast_validation::check(cx, walk));
    let scopes = defs::scopes(cx);
    found.extend(defs::check(cx, &scopes));
    found.extend(names::check(cx, &scopes));
    found.extend(features::early(cx, walk, &owners, features, &flags));
    let lowered = attrs::parsed(cx, &session, &owners, &mut found);
    found.extend(krate::check(cx));
    attrs::checked(cx, &session, &owners, &lowered, &mut found);
    found.extend(features::unused_or_stable(cx, &owners, features));
    found.extend(impls::check(cx, &scopes));
    found
}

/// The modules of `cx.krate` (not those of blocks), in the order rustc visits them: the crate
/// root, then each module before the modules in it, in the order they are declared.
pub(crate) fn modules(cx: &Cx<'_, '_>) -> Vec<hir::Module> {
    let db = cx.db;
    let def_map = hir_def::nameres::crate_def_map(db, cx.krate.base());
    let mut order = Vec::new();
    let mut stack = vec![def_map.root_module_id()];
    while let Some(id) = stack.pop() {
        order.push(hir::Module::from(id));
        let start = stack.len();
        stack.extend(def_map.module(db, id).children(db).values().copied());
        stack[start..].reverse();
    }
    order
}

/// The crate's attributes as rustc reads them: the crate root's `#![..]`, then those of
/// `-Z crate-attr`, with `cfg_attr` expanded as the crate's cfg says.
pub(crate) fn crate_attrs(cx: &Cx<'_, '_>) -> Vec<ast::Meta> {
    let Some(root) = cx.krate.root_module(cx.db).as_source_file_id(cx.db) else { return Vec::new() };
    let tree = root.parse(cx.db).tree();
    let extra = hir_def::attrs::parse_extra_crate_attrs(cx.db, cx.krate.into());
    let attrs = tree.attrs().chain(extra.iter().flat_map(|it| it.attrs()));
    let mut metas = Vec::new();
    hir_expand::attrs::expand_cfg_attr::<()>(attrs, || cx.krate.cfg(cx.db), |meta, _| {
        metas.push(meta);
        ControlFlow::Continue(())
    });
    metas
}

/// Whether the crate has the attribute `#![name]` (or `#![name(..)]`, `#![name = ..]`).
pub(crate) fn has_crate_attr(cx: &Cx<'_, '_>, name: &str) -> bool {
    crate_attrs(cx).iter().any(|meta| {
        meta.path().and_then(|path| path.as_single_name_ref()).is_some_and(|it| it.text() == name)
            && !matches!(meta, ast::Meta::CfgMeta(_) | ast::Meta::CfgAttrMeta(_))
    })
}
