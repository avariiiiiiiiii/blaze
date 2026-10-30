use either::Either;
use syntax::{AstNode, ast};

use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};

// Diagnostic: unresolved-path
//
// This diagnostic is triggered if a path of more than one segment in an expression or a
// pattern resolves to nothing.
pub(crate) fn unresolved_path(ctx: &DiagnosticsContext<'_, '_>, d: &hir::UnresolvedPath) -> Diagnostic {
    let range = ctx.sema.diagnostics_display_range(d.node.map(|it| it.syntax_node_ptr()));
    let root = ctx.sema.parse_or_expand(d.node.file_id);
    let node = d.node.value.to_node(&root);
    let (path, code, what) = match &node {
        Either::Left(ast::Expr::RecordExpr(it)) => (it.path(), "E0422", "struct"),
        Either::Right(ast::Pat::RecordPat(it)) => (it.path(), "E0422", "struct"),
        Either::Right(ast::Pat::TupleStructPat(it)) => (it.path(), "E0531", "tuple struct or variant"),
        Either::Right(ast::Pat::PathPat(it)) => (it.path(), "E0531", "unit struct, variant or constant"),
        Either::Left(ast::Expr::PathExpr(it)) => (it.path(), "E0425", "value"),
        _ => (None, "E0425", "value"),
    };
    let prefix = path.as_ref().is_some_and(|path| {
        // `segment` counts the segments after `crate`, `self`, `super` and `$crate`.
        let named = path.segments().filter(|it| matches!(it.kind(), Some(ast::PathSegmentKind::Name(_)))).count();
        (d.segment as usize) + 1 < named
    });
    match prefix {
        true => Diagnostic::new(DiagnosticCode::RustcHardError("E0433"), "failed to resolve a prefix of this path", range),
        false => Diagnostic::new(DiagnosticCode::RustcHardError(code), format!("no such {what} in this path's module"), range),
    }
}

#[cfg(test)]
mod tests {
    use crate::tests::check_diagnostics;

    #[test]
    fn last_segment() {
        check_diagnostics(
            r#"
mod m {}
fn main() {
    let _ = m::x;
          //^^^^ error: no such value in this path's module
}
"#,
        );
    }

    #[test]
    fn prefix() {
        check_diagnostics(
            r#"
fn main() {
    let _ = n::x;
          //^^^^ error: failed to resolve a prefix of this path
}
"#,
        );
    }
}
