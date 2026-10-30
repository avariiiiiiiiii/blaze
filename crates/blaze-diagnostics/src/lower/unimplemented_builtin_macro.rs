//! A `#[rustc_builtin_macro]` blaze has no expander for. rustc expands it; blaze would leave
//! every call of it unexpanded, so it is blaze's own error at the definition, naming the macro
//! (no-fallback), never nothing.

use syntax::{AstNode, ast};

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::UnimplementedBuiltinMacro) -> Vec<Found> {
    let node = cx.syntax(d.node);
    let name = match ast::Name::cast(node.value.clone()) {
        Some(name) => name.text().to_owned(),
        None => node.value.to_string(),
    };
    let diag = Diag::error(
        None,
        format!("blaze does not expand the built-in macro `{name}` yet"),
        cx.span_of(d.node),
    );
    vec![Found::new(diag, Phase::Expand)]
}
