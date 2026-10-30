use crate::{Diagnostic, DiagnosticCode, DiagnosticsContext};

// Diagnostic: fully-qualified-call-from-2018
//
// This diagnostic is triggered before Rust 2021 by a type-relative path to `try_from`,
// `try_into` or `from_iter` of a trait the 2021 prelude makes ambiguous with `TryFrom`,
// `TryInto` or `FromIterator` (`rust_2021_prelude_collisions`, allowed by default).
pub(crate) fn fully_qualified_call_from_2018<'db>(
    ctx: &DiagnosticsContext<'_, 'db>,
    d: &hir::FullyQualifiedCallFrom2018<'db>,
) -> Diagnostic {
    let name = d.item.name(ctx.sema.db);
    let name = name.as_ref().map_or("", |it| it.as_str());
    Diagnostic::new_with_syntax_node_ptr(
        ctx,
        DiagnosticCode::RustcLint("rust_2021_prelude_collisions"),
        format!("trait-associated function `{name}` will become ambiguous in Rust 2021"),
        d.expr_or_pat.map(Into::into),
    )
    .stable()
}

#[cfg(test)]
mod tests {
    use crate::tests::check_diagnostics;

    #[test]
    fn try_from_of_a_local_trait_before_2021() {
        check_diagnostics(
            r#"
//- /main.rs crate:main edition:2018
#![warn(rust_2021_prelude_collisions)]
trait TryFromU8 {
    fn try_from(x: u8) -> Self;
}
struct S;
impl TryFromU8 for S {
    fn try_from(_: u8) -> S {
        S
    }
}
fn main() {
    let _ = S::try_from(3);
          //^^^^^^^^^^^ warn: trait-associated function `try_from` will become ambiguous in Rust 2021
}
"#,
        );
    }
}
