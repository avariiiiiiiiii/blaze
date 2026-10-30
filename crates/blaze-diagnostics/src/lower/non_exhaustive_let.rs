//! E0005: a `let` whose pattern does not cover every value of what it binds.
//!
//! Follows rustc's `compiler/rustc_mir_build/src/thir/pattern/check_match.rs` (`check_let`,
//! `check_binding_is_irrefutable`, `is_const_pat_that_looks_like_binding`,
//! `report_adt_defined_here`), `compiler/rustc_mir_build/src/diagnostics.rs`
//! (`PatternNotCovered`, `Inform`, `InterpretedAsConst`, `InterpretedAsConstSugg`, `SuggestLet`,
//! `MiscPatternSuggestion`) and `compiler/rustc_pattern_analysis/src/diagnostics.rs`
//! (`Uncovered`) (MIT or Apache-2.0). The witnesses are read back as for E0004
//! ([`super::missing_match_arms`]). hir gives this of `let` statements only: rustc's other
//! origins ("function argument", "closure argument", "`for` loop binding", "assignment") are
//! asked of db in `v2/requests/diagnostics.md`.

use syntax::{AstNode, TextRange, ast};

use super::missing_match_arms::{
    DefinedHere, Uncovered, Witnesses, adt_fact, all_chars, const_that_looks_like_binding, plural,
};
use crate::{
    cx::Cx,
    lints::code_fact,
    lower::{Found, Phase},
    model::{Applicability, Diag},
};

pub(crate) fn lower<'db>(cx: &Cx<'_, 'db>, d: &hir::NonExhaustiveLet) -> Vec<Found> {
    let pat = cx.node(d.pat);
    let span = cx.span_of_ptr(d.pat);
    let stmt = pat.value.syntax().parent().and_then(ast::LetStmt::cast);
    let init = stmt.as_ref().and_then(|it| it.initializer());
    let ty = match &init {
        Some(init) => cx.sema.type_of_expr(init).map(|it| it.adjusted()),
        None => cx.sema.type_of_pat(&pat.value).map(|it| it.original()),
    };
    let witnesses = match Uncovered::read(&d.uncovered_patterns) {
        Some(Uncovered::Witnesses { shown, count }) => Witnesses::read(cx, &shown, count, ty.as_ref()),
        _ => Witnesses { read: Vec::new(), printed: Vec::new(), count: 0 },
    };

    let mut diag = Diag::error(Some("E0005"), "refutable pattern in local binding", span.clone());
    match witnesses.count {
        0 => diag.span_label(span.clone(), d.uncovered_patterns.clone()),
        count => diag.span_label(span.clone(), format!("pattern{} {} not covered", plural(count), witnesses.joined())),
    };
    diag.note("`let` bindings require an \"irrefutable pattern\", like a `struct` or an `enum` with only one variant");
    diag.note("for more information, visit https://doc.rust-lang.org/book/ch19-02-refutability.html");

    let constant = match &pat.value {
        ast::Pat::IdentPat(it) => const_that_looks_like_binding(cx, it),
        _ => None,
    };
    if let Some((name, def)) = &constant {
        diag.span_label(
            def.clone(),
            format!(
                "missing patterns are not covered because `{name}` is interpreted as a constant pattern, not a new variable"
            ),
        );
        diag.suggestion_verbose(
            span.clone(),
            "introduce a variable instead",
            format!("{name}_var"),
            Applicability::MaybeIncorrect,
        );
    }
    if let Some(ty) = &ty
        && let Some(here) = DefinedHere::of(cx, ty, &witnesses.read, false)
    {
        diag.span_note(here.span(false), here.message());
    }
    if let Some(ty) = &ty {
        diag.note(format!("the matched value is of type `{}`", cx.ty(ty)));
    }

    if constant.is_none()
        && let Some(stmt) = &stmt
        && let Some(init) = &init
    {
        // rustc's span of the statement: from `let` (or `super`) to the end of its initializer.
        let keyword = stmt.super_token().or_else(|| stmt.let_token());
        let lo = keyword.map_or(stmt.syntax().text_range().start(), |it| it.text_range().start());
        let hi = init.syntax().text_range().end();
        let whole = cx.span(pat.with_value(TextRange::new(lo, hi.max(lo))));
        let (start, end) = (whole.shrink_to_lo(), whole.shrink_to_hi());
        let variants = match witnesses.count {
            1 => "variant that isn't",
            _ => "variants that aren't",
        };
        let has_bindings = pat
            .value
            .syntax()
            .descendants()
            .filter_map(ast::IdentPat::cast)
            .any(|it| cx.sema.resolve_bind_pat_to_const(&it).is_none());
        match has_bindings {
            true => {
                diag.suggestion(
                    end,
                    format!("you might want to use `let...else` to handle the {variants} matched"),
                    " else { todo!() }",
                    Applicability::HasPlaceholders,
                );
            }
            false => {
                diag.multipart_suggestion(
                    format!("you might want to use `if let` to ignore the {variants} matched"),
                    vec![(start, "if ".to_owned()), (end, " { todo!() }".to_owned())],
                    Applicability::HasPlaceholders,
                );
            }
        }
    }
    if constant.is_none()
        && let ast::Pat::LiteralPat(literal) = &pat.value
        && all_chars(literal.syntax(), |c| c.is_ascii_digit())
    {
        diag.suggestion(
            span.shrink_to_lo(),
            "alternatively, you could prepend the pattern with an underscore to define a new named variable; identifiers cannot begin with digits",
            "_",
            Applicability::MaybeIncorrect,
        );
    }

    if let Some(ty) = &ty {
        adt_fact(cx, &mut diag, ty);
    }
    code_fact(cx, &mut diag, d.pat.map(|it| it.syntax_node_ptr()));
    vec![Found::new(diag, Phase::Mir)]
}
