use hir::HirDisplay;
use ide_db::Severity;
use syntax::ast;

use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};

// Diagnostic: method-call-illegal-sized-bound
//
// This diagnostic is triggered when a method is called on a trait-object
// receiver but the method's predicates require `Self: Sized`, which the
// trait object cannot satisfy.
pub(crate) fn method_call_illegal_sized_bound(
    ctx: &DiagnosticsContext<'_, '_>,
    d: &hir::MethodCallIllegalSizedBound,
) -> Diagnostic {
    let name = d.method.name(ctx.sema.db);
    let name = name.display(ctx.sema.db, ctx.edition);
    let message = match d.needs_mut.then(|| receiver_ty(ctx, d)).flatten() {
        Some(ty) => format!(
            "the `{name}` method cannot be invoked on `{}`",
            ty.display(ctx.sema.db, ctx.display_target)
        ),
        None => format!("the `{name}` method cannot be invoked on a trait object"),
    };
    Diagnostic::new_with_syntax_node_ptr(
        ctx,
        DiagnosticCode::Ra("method-call-illegal-sized-bound", Severity::Error),
        message,
        d.call_expr.map(|it| it.into()),
    )
    .stable()
}

fn receiver_ty<'db>(
    ctx: &DiagnosticsContext<'_, 'db>,
    d: &hir::MethodCallIllegalSizedBound,
) -> Option<hir::Type<'db>> {
    let root = d.call_expr.file_id.parse_or_expand(ctx.sema.db);
    let ast::Expr::MethodCallExpr(call) = d.call_expr.value.to_node(&root).left()? else {
        return None;
    };
    Some(ctx.sema.type_of_expr(&call.receiver()?)?.original)
}

#[cfg(test)]
mod tests {
    use crate::tests::check_diagnostics;

    #[test]
    fn sized_bound_method_on_trait_object_errors() {
        check_diagnostics(
            r#"
//- minicore: sized
trait Foo {
    fn cant_call(&self) where Self: Sized;
}

fn f(x: &dyn Foo) {
    x.cant_call();
  //^^^^^^^^^^^^^ error: the `cant_call` method cannot be invoked on a trait object
}
"#,
        );
    }

    #[test]
    fn method_without_sized_bound_on_trait_object_does_not_error() {
        check_diagnostics(
            r#"
//- minicore: sized
trait Foo {
    fn dyn_safe(&self);
}

fn f(x: &dyn Foo) {
    x.dyn_safe();
}
"#,
        );
    }

    #[test]
    fn sized_bound_method_on_concrete_type_does_not_error() {
        check_diagnostics(
            r#"
//- minicore: sized
trait Foo {
    fn cant_dispatch(&self) where Self: Sized;
}

struct S;
impl Foo for S {
    fn cant_dispatch(&self) {}
}

fn f(s: &S) {
    s.cant_dispatch();
}
"#,
        );
    }

    #[test]
    fn regression_23175() {
        check_diagnostics(
            r#"
//- minicore: sized
trait Iterator {
    type Item;

    fn foo(self)
    where
        Self: Sized,
    {
    }
}
impl<T: ?Sized + Iterator> Iterator for &mut T {
    type Item = T::Item;
}

trait Helper:
    Iterator<
    Item = [(); {
               {}
               4
           }],
>
{
}

fn x(w: &mut dyn Helper) {
    w.foo();
}
        "#,
        );
    }
}
