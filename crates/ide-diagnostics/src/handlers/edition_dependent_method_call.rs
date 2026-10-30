use hir::EditionLint;

use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};

// Diagnostic: edition-dependent-method-call
//
// This diagnostic is triggered when a method call resolves to another method in a later
// edition: a trait that edition's prelude adds (`rust_2021_prelude_collisions`,
// `rust_2024_prelude_collisions`), or `into_iter` on an array (`array_into_iter`) or a boxed
// slice (`boxed_slice_into_iter`).
pub(crate) fn edition_dependent_method_call(
    ctx: &DiagnosticsContext<'_, '_>,
    d: &hir::EditionDependentMethodCall,
) -> Diagnostic {
    let lint = match d.lint {
        EditionLint::Rust2021PreludeCollisions => "rust_2021_prelude_collisions",
        EditionLint::Rust2024PreludeCollisions => "rust_2024_prelude_collisions",
        EditionLint::ArrayIntoIter => "array_into_iter",
        EditionLint::BoxedSliceIntoIter => "boxed_slice_into_iter",
    };
    let edition = match d.lint {
        EditionLint::Rust2021PreludeCollisions | EditionLint::ArrayIntoIter => "2021",
        EditionLint::Rust2024PreludeCollisions | EditionLint::BoxedSliceIntoIter => "2024",
    };
    let name = d.method.name(ctx.sema.db);
    // An inherent pick is linted on the receiver, which the fix adjusts; a trait's on the call.
    let node = match d.inherent {
        true => d.receiver.map(Into::into),
        false => d.call_expr.map(Into::into),
    };
    Diagnostic::new_with_syntax_node_ptr(
        ctx,
        DiagnosticCode::RustcLint(lint),
        format!("trait method `{}` will become ambiguous in Rust {edition}", name.as_str()),
        node,
    )
    .stable()
}

#[cfg(test)]
mod tests {
    use crate::tests::check_diagnostics;

    #[test]
    fn trait_into_iter_on_an_array_before_2021() {
        check_diagnostics(
            r#"
//- /main.rs crate:main edition:2018
struct FooIter;
trait MyIntoIter {
    fn into_iter(self) -> FooIter;
}
impl<T, const N: usize> MyIntoIter for [T; N] {
    fn into_iter(self) -> FooIter {
        FooIter
    }
}
fn main() {
    let points: [u8; 1] = [0];
    let _y = points.into_iter();
           //^^^^^^^^^^^^^^^^^^ warn: trait method `into_iter` will become ambiguous in Rust 2021
}
"#,
        );
    }

    #[test]
    fn trait_into_iter_on_an_array_in_2021() {
        check_diagnostics(
            r#"
//- /main.rs crate:main edition:2021
struct FooIter;
trait MyIntoIter {
    fn into_iter(self) -> FooIter;
}
impl<T, const N: usize> MyIntoIter for [T; N] {
    fn into_iter(self) -> FooIter {
        FooIter
    }
}
fn main() {
    let points: [u8; 1] = [0];
    let _y = points.into_iter();
}
"#,
        );
    }
}
