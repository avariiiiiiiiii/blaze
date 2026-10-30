//! E0033: a `&` or `box` pattern binding what a trait object points to, as `let &x = obj;`
//! with `obj: &dyn Trait`, which would move an unsized value.
//!
//! Follows rustc's `compiler/rustc_hir_typeck/src/pat.rs` (`check_dereferenceable`) (MIT or
//! Apache-2.0). Its note, `CANNOT_IMPLICITLY_DEREF_POINTER_TRAIT_OBJ`, is written only under
//! `-Z teach`, which blaze is not run with.

use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower<'db>(
    cx: &Cx<'_, 'db>,
    d: &hir::CannotImplicitlyDerefTraitObject<'db>,
) -> Vec<Found> {
    let span = cx.span_of_ptr(d.pat);
    let ty = cx.ty(&d.found);
    let mut diag =
        Diag::error(Some("E0033"), format!("type `{ty}` cannot be dereferenced"), span.clone());
    diag.span_label(span, format!("type `{ty}` cannot be dereferenced"));
    code_fact(cx, &mut diag, d.pat.map(|it| it.syntax_node_ptr()));
    vec![Found::new(diag, Phase::Bodies)]
}
