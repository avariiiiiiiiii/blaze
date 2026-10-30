//! What rustc's borrow checker gives: its errors (E0499, E0502, E0505, E0382, E0596, E0597,
//! E0716, E0384, .., and the ones with no code, "lifetime may not live long enough"), and the
//! lint `unused_mut`.
//!
//! Follows rustc's `compiler/rustc_borrowck/src/borrowck_errors.rs` and
//! `compiler/rustc_borrowck/src/lib.rs` (`lint_unused_mut`) with
//! `compiler/rustc_borrowck/src/session_diagnostics.rs` (`VarNeedNotMut`) (MIT or
//! Apache-2.0). hir gives an error's code, its message in rustc's words and its primary span
//! (`crates/hir-ty-borrowck/src/check.rs`); the labels and notes rustc puts on its other spans
//! (the first borrow, the later use, the move, the type that is not `Copy`) are not given
//! yet, and are not written rather than guessed.

use syntax::{AstNode, SyntaxKind, TextRange, ast};

use crate::{
    cx::Cx,
    lints::{code_fact, registry::LintId},
    lower::{Found, const_check_error::phase},
    model::{Applicability, Diag, SuggestionStyle},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::BorrowckError) -> Vec<Found> {
    if d.code == "unused_mut" {
        return unused_mut(cx, d);
    }
    let node = cx.syntax(d.node);
    let range = match d.at_end {
        true => node.value.last_token().map_or(node.value.text_range(), |it| it.text_range()),
        false => node.value.text_range(),
    };
    let span = cx.span(node.with_value(range));
    let mut diag = match d.code.starts_with(|it: char| it.is_ascii_lowercase()) {
        true => Diag::lint(d.code, d.message.clone(), span),
        false => Diag::error((!d.code.is_empty()).then_some(d.code), d.message.clone(), span),
    };
    code_fact(cx, &mut diag, d.node);
    let phase = phase(cx, d.node);
    match diag.is_lint() {
        true => vec![Found::lint(diag, phase, d.node)],
        false => vec![Found::new(diag, phase)],
    }
}

/// rustc's `VarNeedNotMut`, at the binding's pattern, with the suggestion to remove `mut `
/// (the source map's `span_until_non_whitespace` of the pattern).
fn unused_mut(cx: &Cx<'_, '_>, d: &hir::BorrowckError) -> Vec<Found> {
    const LINT: LintId = LintId::named("unused_mut");
    if !cx.lint_may_fire(LINT) {
        return Vec::new();
    }
    let node = cx.syntax(d.node);
    let start = node.value.text_range().start();
    let mut_token = match ast::SelfParam::cast(node.value.clone()) {
        Some(it) => it.mut_token(),
        None => ast::IdentPat::cast(node.value.clone()).and_then(|it| it.mut_token()),
    };
    // `mut` and the white space after it, up to what comes next.
    let mut_range = match mut_token {
        Some(token) if token.text_range().start() == start => {
            let mut end = token.text_range().end();
            let mut next = token.next_token();
            while let Some(it) = next.filter(|it| it.kind() == SyntaxKind::WHITESPACE) {
                end = it.text_range().end();
                next = it.next_token();
            }
            TextRange::new(start, end)
        }
        _ => TextRange::empty(start),
    };
    let span = cx.span_of(d.node);
    let mut diag = Diag::lint("unused_mut", "variable does not need to be mutable", span);
    if !mut_range.is_empty() {
        diag.suggestion_with_style(
            vec![(cx.span(node.with_value(mut_range)), String::new())],
            "remove this `mut`",
            Applicability::MachineApplicable,
            SuggestionStyle::HideCodeInline,
        );
    }
    code_fact(cx, &mut diag, d.node);
    vec![Found::lint(diag, phase(cx, d.node), d.node)]
}
