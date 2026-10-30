//! What rustc lints on a macro's definition, each read in the edition of its tokens' spans:
//!
//! - `meta_variable_misuse`: a variable of a rule's transcriber that its matcher does not bind,
//!   or uses at another depth or with another repetition operator; buffered at the definition.
//! - `rust_2021_incompatible_or_patterns`: a `pat` before 2021 that `|` may follow; buffered at
//!   the crate.
//! - `edition_2024_expr_fragment_specifier`: an `expr` before 2024, of the early lint pass's
//!   `check_mac_def`.
//!
//! Follows rustc's `compiler/rustc_expand/src/mbe/macro_rules.rs` (`compile_declarative_macro`),
//! `compiler/rustc_expand/src/mbe/macro_check.rs`, `compiler/rustc_expand/src/mbe/quoted.rs`,
//! `compiler/rustc_lint/src/macro_expr_fragment_specifier_2024_migration.rs`, with the text of
//! `compiler/rustc_expand/src/diagnostics.rs` and `compiler/rustc_lint/src/lints.rs`
//! (`MacroExprFragment2024`), and `compiler/rustc_span/src/symbol.rs` (`Ident`'s `Display`)
//! (MIT or Apache-2.0).

mod check;
mod expr_2024;
mod follow;
mod quoted;
mod tokens;

use hir::{HirFileId, InFile};
use rustc_hash::FxHashSet;
use span::{Edition, SyntaxContext};
use syntax::{AstNode, TextRange, TextSize, ast};

use self::{
    check::{Checker, Misuse},
    quoted::Matchers,
    tokens::{Token, Tokens},
};
use crate::{
    cx::Cx,
    lints::syntax::{Emitted, Pass, Tree, Walk, source_fact},
    lower::Phase,
    model::{Applicability, Diag},
};

pub(super) fn lints(cx: &Cx<'_, '_>, walk: &Walk<'_>) -> Vec<Emitted> {
    let krate = cx
        .krate
        .root_module(cx.db)
        .as_source_file_id(cx.db)
        .and_then(|it| walk.tree(HirFileId::from(it)))
        .map(|it| it.ptr(&it.root));
    let mut out = Vec::new();
    let mut or_patterns: FxHashSet<(HirFileId, TextRange)> = FxHashSet::default();
    for tree in &walk.trees {
        for node in &tree.nodes {
            let Some(def) = ast::Macro::cast(node.clone()) else { continue };
            let at = tree.ptr(node);
            let span = |offset: TextSize| {
                let ctx = match tree.file.macro_file() {
                    None => SyntaxContext::root(tree.file.edition(cx.db)),
                    Some(call) => call.expansion_span_map(cx.db).span_at(offset).ctx,
                };
                (ctx.edition(cx.db), ctx.is_root())
            };
            let Some(tokens) = Tokens::of(&def, &span) else { continue };
            let (macro_rules, keyword) = match &def {
                ast::Macro::MacroRules(it) => (true, it.macro_rules_token()),
                ast::Macro::MacroDef(it) => (false, it.macro_token()),
            };
            let start = keyword.map_or(node.text_range().start(), |it| it.text_range().start());
            let (m, rules) = Matchers::rules(&tokens, macro_rules, span(start).0);

            let mut checker = Checker::new(&m, &tokens);
            let mut found = Vec::new();
            for rule in &rules {
                for lhs in rule.args.into_iter().chain(rule.lhs) {
                    follow::check_lhs(&m, &tokens, lhs, &mut found);
                }
                if let (Some(lhs), Some(rhs)) = (rule.lhs, rule.rhs) {
                    checker.rule(rule.args, lhs, rhs);
                }
            }
            for misuse in &checker.misuses {
                let mut diag = misuse_diag(cx, tree, &tokens, misuse);
                source_fact(cx, &mut diag, at);
                out.push(Emitted::new(diag, Phase::Resolve, Pass::Buffered, at));
            }
            if let Some(krate) = krate {
                for it in found {
                    if !or_patterns.insert((tree.file, it.span)) {
                        continue;
                    }
                    let span = cx.span(InFile::new(tree.file, it.span));
                    let mut diag = Diag::lint(
                        "rust_2021_incompatible_or_patterns",
                        "the meaning of the `pat` fragment specifier is changing in Rust 2021, which may affect this macro",
                        span.clone(),
                    );
                    diag.suggestion(
                        span,
                        "use pat_param to preserve semantics",
                        format!("${}:pat_param", printed(&tokens, it.name)),
                        Applicability::MachineApplicable,
                    );
                    source_fact(cx, &mut diag, krate);
                    out.push(Emitted::new(diag, Phase::Resolve, Pass::Buffered, krate));
                }
            }
            for range in expr_2024::exprs(&tokens) {
                let span = cx.span(InFile::new(tree.file, range));
                let mut diag = Diag::lint(
                    "edition_2024_expr_fragment_specifier",
                    "the `expr` fragment specifier will accept more expressions in the 2024 edition",
                    span.clone(),
                );
                diag.suggestion(
                    span,
                    "to keep the existing behavior, use the `expr_2021` fragment specifier",
                    "expr_2021",
                    Applicability::MachineApplicable,
                );
                source_fact(cx, &mut diag, at);
                out.push(Emitted::new(diag, Phase::Resolve, Pass::Early, at));
            }
        }
    }
    out
}

fn misuse_diag(cx: &Cx<'_, '_>, tree: &Tree, tokens: &Tokens, misuse: &Misuse) -> Diag {
    let span = |range: TextRange| cx.span(InFile::new(tree.file, range));
    match *misuse {
        Misuse::Duplicate { span: at, prev } => {
            let mut diag = Diag::lint("meta_variable_misuse", "duplicate matcher binding", span(at));
            diag.span_label(span(at), "duplicate binding");
            diag.span_label(span(prev), "previous binding");
            diag
        }
        Misuse::Unknown { span: at, name } => Diag::lint(
            "meta_variable_misuse",
            format!("unknown macro variable `{}`", printed(tokens, name)),
            span(at),
        ),
        Misuse::StillRepeating { span: at, binder, name } => {
            let mut diag = Diag::lint(
                "meta_variable_misuse",
                format!("variable `{}` is still repeating at this depth", printed(tokens, name)),
                span(at),
            );
            diag.span_label(span(binder), "expected repetition");
            diag
        }
        Misuse::WrongOperator { span: at, binder, occurrence } => {
            let mut diag =
                Diag::lint("meta_variable_misuse", "meta-variable repeats with different Kleene operator", span(at));
            diag.span_label(span(binder), "expected repetition");
            diag.span_label(span(occurrence), "conflicting repetition");
            diag
        }
    }
}

/// An identifier as rustc's `Ident` displays it: with `r#` when its name is reserved in its
/// span's edition and can be raw (`is_raw_guess`).
fn printed(tokens: &Tokens, ident: Token) -> String {
    let name = tokens.name(ident.range);
    let reserved = match name {
        "as" | "break" | "const" | "continue" | "else" | "enum" | "extern" | "false" | "fn" | "for" | "if"
        | "impl" | "in" | "let" | "loop" | "match" | "mod" | "move" | "mut" | "pub" | "ref" | "return"
        | "static" | "struct" | "trait" | "true" | "type" | "unsafe" | "use" | "where" | "while"
        | "abstract" | "become" | "box" | "do" | "final" | "macro" | "override" | "priv" | "typeof"
        | "unsized" | "virtual" | "yield" => true,
        "async" | "await" | "dyn" | "try" => ident.edition >= Edition::Edition2018,
        "gen" => ident.edition >= Edition::Edition2024,
        _ => false,
    };
    match reserved {
        true => format!("r#{name}"),
        false => name.to_owned(),
    }
}
