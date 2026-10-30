//! What match checking lints: `unreachable_patterns`, `irrefutable_let_patterns` and
//! `bindings_with_variant_name` (E0170).
//!
//! Follows `compiler/rustc_mir_build/src/thir/pattern/check_match.rs`
//! (`report_unreachable_pattern`, `report_arm_reachability`, `report_irrefutable_let_patterns`,
//! `check_let`, `check_for_bindings_named_same_as_variants`) and
//! `compiler/rustc_mir_build/src/diagnostics.rs` (`UnreachablePatternInner`, `WantedConstant`,
//! `IrrefutableLetPatterns*`, `LetElseReplacementSuggestion`, `BindingsWithVariantName`) (MIT or
//! Apache-2.0). Which patterns are redundant and why is match checking's
//! (`hir::DefWithBody::match_lints`); rustc checks no body that failed type checking.

use hir::{DefWithBody, HasSource as _, InFile, IrrefutableLet, LetSource, MatchLint, PatternTypo};
use syntax::{
    AstNode, AstPtr, SyntaxNodePtr, TextRange,
    ast::{self, HasName as _},
};

use super::BodyCx;
use crate::{
    cx::def_range,
    lower::{Found, Phase},
    model::{Applicability, Diag, MultiSpan, Span},
};

/// rustc's `CAP_COVERED_BY_MANY`.
const COVERED_BY_MANY: usize = 4;

/// The lints this module gives: it runs only when one may fire.
pub(super) const LINTS: super::LintSet =
    super::lint_set(["unreachable_patterns", "irrefutable_let_patterns", "bindings_with_variant_name"]);

pub(super) fn lints(b: &BodyCx<'_, '_>) -> Vec<Found> {
    if b.tainted() {
        return Vec::new();
    }
    let lints = DefWithBody::from(b.owner).match_lints(b.db());
    lints
        .iter()
        .filter_map(|lint| match lint {
            MatchLint::UnreachablePattern(it) => unreachable_pattern(b, it),
            MatchLint::IrrefutableLet(it) => irrefutable_let(b, it),
            MatchLint::BindingWithVariantName(it) => binding_with_variant_name(b, it),
        })
        .collect()
}

fn unreachable_pattern(b: &BodyCx<'_, '_>, d: &hir::UnreachablePattern<'_>) -> Option<Found> {
    let cx = b.cx;
    let at = pat_node(d.pat);
    let pat = cx.span_of(at);
    let mut diag = Diag::lint("unreachable_patterns", "unreachable pattern", pat.clone());
    let covering: Vec<Span> = d.covered_by.iter().map(|it| cx.span_of_ptr(*it)).collect();
    match covering.as_slice() {
        [] => {
            diag.span_label(pat.clone(), format!("matches no values because `{}` is uninhabited", cx.ty(&d.ty)));
            diag.note(
                "to learn more about uninhabited types, see https://doc.rust-lang.org/nomicon/exotic-sizes.html#empty-types",
            );
        }
        [covering] if d.catchall => {
            diag.span_label(pat.clone(), "no value can reach this");
            diag.span_label(covering.clone(), "matches any value");
            if let Some(typo) = &d.typo {
                pattern_typo(b, &mut diag, &pat, typo);
            }
        }
        [covering] => {
            diag.span_label(pat.clone(), "no value can reach this");
            diag.span_label(covering.clone(), "matches all the relevant values");
        }
        covering => {
            diag.span_label(pat.clone(), "no value can reach this");
            let mut spans = MultiSpan::of(pat.clone());
            for it in covering.iter().take(COVERED_BY_MANY) {
                spans.labels.push((it.clone(), "matches some of the same values".to_owned()));
            }
            let label = match covering.len().saturating_sub(COVERED_BY_MANY) {
                0 => "collectively making this unreachable".to_owned(),
                remain => format!("...and {remain} other patterns collectively make this unreachable"),
            };
            spans.labels.push((pat.clone(), label));
            diag.span_note(spans, "multiple earlier patterns match some of the same values");
        }
    }
    if covering.is_empty()
        && let Some(arm) = d.arm
    {
        diag.suggestion(whole_arm(b, arm)?, "remove the match arm", "", Applicability::MachineApplicable);
    }
    Some(b.lint(diag, Phase::Mir, at))
}

/// `find_fallback_pattern_typo`'s part of the lint.
fn pattern_typo(b: &BodyCx<'_, '_>, diag: &mut Diag, pat: &Span, typo: &PatternTypo) {
    let cx = b.cx;
    match typo {
        PatternTypo::WantedConstant { is_typo, const_name, const_path } => {
            let what = if *is_typo { "similarly named constant" } else { "constant" };
            diag.suggestion(
                pat.clone(),
                format!(
                    "you might have meant to pattern match against the value of {what} `{const_name}` instead of introducing a new catch-all binding"
                ),
                const_path.clone(),
                Applicability::MachineApplicable,
            );
        }
        PatternTypo::AccessibleConstant(name) => {
            diag.span_note(
                cx.span(*name),
                "there is a constant of the same name imported in another scope, which could have been used to pattern match against its value instead of introducing a new catch-all binding, but it needs to be imported in the pattern's scope",
            );
        }
        PatternTypo::InaccessibleConstant(konst) => {
            let Some(source) = konst.source(cx.db) else { return };
            let node = source.map(|it| it.syntax().clone());
            let range = def_range(&node.value);
            diag.span_note(
                cx.span(InFile::new(node.file_id, range)),
                "there is a constant of the same name, which could have been used to pattern match against its value instead of introducing a new catch-all binding, but it is not accessible from this scope",
            );
        }
        PatternTypo::PatternLetBinding(name) => {
            diag.span_note(
                cx.span(*name),
                "there is a binding of the same name; if you meant to pattern match against the value of that binding, that is a feature of constants that is not available for `let` bindings",
            );
        }
    }
}

/// rustc's `whole_arm_span`: the arm, from its pattern to its value, with the `,` after it.
fn whole_arm(b: &BodyCx<'_, '_>, arm: InFile<AstPtr<ast::MatchArm>>) -> Option<Span> {
    let node = b.cx.node(arm);
    let start = node.value.pat()?.syntax().text_range().start();
    let end = match node.value.comma_token() {
        Some(comma) => comma.text_range().end(),
        None => node.value.expr()?.syntax().text_range().end(),
    };
    Some(b.cx.span(InFile::new(node.file_id, TextRange::new(start, end))))
}

fn irrefutable_let(b: &BodyCx<'_, '_>, d: &IrrefutableLet) -> Option<Found> {
    let cx = b.cx;
    let (message, note, help) = match d.source {
        LetSource::IfLet => (
            "irrefutable `if let` pattern",
            "this pattern will always match, so the `if let` is useless",
            "consider replacing the `if let` with a `let`",
        ),
        LetSource::IfLetGuard => (
            "irrefutable `if let` guard pattern",
            "this pattern will always match, so the guard is useless",
            "consider removing the guard and adding a `let` inside the match arm",
        ),
        LetSource::WhileLet => (
            "irrefutable `while let` pattern",
            "this pattern will always match, so the loop will never exit",
            "consider instead using a `loop { ... }` with a `let` inside it",
        ),
        LetSource::LetElse => return let_else(b, d),
    };
    let mut diag = Diag::lint("irrefutable_let_patterns", message, cx.span_of(d.at));
    diag.note(note).help(help);
    Some(b.lint(diag, Phase::Mir, d.at))
}

/// `LetSource::LetElse`: at `else`, with the `let` labelled, unless the `let` is a macro's.
fn let_else(b: &BodyCx<'_, '_>, d: &IrrefutableLet) -> Option<Found> {
    let cx = b.cx;
    let node = cx.syntax(d.at);
    let stmt = ast::LetStmt::cast(node.value)?;
    let start = stmt.let_token()?.text_range().start();
    let end = stmt.initializer()?.syntax().text_range().end();
    let span = cx.span(InFile::new(node.file_id, TextRange::new(start, end)));
    let mut diag = match span.expansion.is_some() {
        true => Diag::lint("irrefutable_let_patterns", "unreachable `else` clause", span.clone()),
        false => {
            let keyword = cx.span(InFile::new(node.file_id, stmt.let_else()?.else_token()?.text_range()));
            let text = cx.file_text(cx.file_of(&keyword)?).get(keyword.lo as usize..)?;
            let len = text.find(char::is_whitespace).unwrap_or(text.len());
            let keyword = keyword.with_hi(keyword.lo + len as u32);
            let mut diag = Diag::lint("irrefutable_let_patterns", "unreachable `else` clause", keyword);
            diag.span_label(span.clone(), "assigning to binding pattern will always succeed");
            diag
        }
    };
    diag.note("this pattern always matches, so the else clause is unreachable");
    if let Some((pat, inner)) = d.replacement
        && let Some(lhs) = cx.snippet(&cx.span_of_ptr(pat))
        && let Some(rhs) = cx.snippet(&cx.span_of_ptr(inner))
    {
        diag.suggestion(
            span,
            format!("consider using `let Some({lhs}) = {rhs}` to match on a specific variant"),
            format!("let Some({lhs}) = {rhs}"),
            Applicability::MachineApplicable,
        );
    }
    Some(b.lint(diag, Phase::Mir, d.at))
}

fn binding_with_variant_name(b: &BodyCx<'_, '_>, d: &hir::BindingWithVariantName) -> Option<Found> {
    let cx = b.cx;
    let at = d.pat.map(|it| it.syntax_node_ptr());
    let name = cx.node(d.pat).value.name()?.text().to_string();
    let span = cx.span_of(at);
    let ty_path = crate::print::def_path_untrimmed(cx, hir::ModuleDef::Adt(d.enum_.into()));
    let mut diag = Diag::lint(
        "bindings_with_variant_name",
        format!("pattern binding `{name}` is named the same as one of the variants of the type `{ty_path}`"),
        span.clone(),
    );
    if d.suggest {
        diag.suggestion(
            span,
            "to match on the variant, qualify the path",
            format!("{ty_path}::{name}"),
            Applicability::MachineApplicable,
        );
    }
    Some(b.lint(diag, Phase::Mir, at))
}

fn pat_node(pat: InFile<AstPtr<ast::Pat>>) -> InFile<SyntaxNodePtr> {
    pat.map(|it| it.syntax_node_ptr())
}
