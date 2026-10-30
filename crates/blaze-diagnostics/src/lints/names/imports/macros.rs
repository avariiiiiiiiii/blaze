//! `unused_macros` and `unused_macro_rules`: the crate's macros no call invokes, and the
//! rules of an invoked `macro_rules!` no call matched.
//!
//! A macro counts: a `macro_rules!` without `#[macro_export]`, or a `macro` that is not `pub`,
//! whose name does not start with `_`. A macro is used when a call, attribute or derive
//! resolves to it; one a call of its name that does not resolve may invoke is not linted, nor
//! are the rules of a macro some call of which matched a rule that can't be told.
//!
//! Follows rustc's `compiler/rustc_resolve/src/build_reduced_graph.rs` (`define_macro`,
//! `insert_unused_macro`), `compiler/rustc_resolve/src/macros.rs` (`check_unused_macros`,
//! `record_macro_rule_usage`, `resolve_macro_or_delegation_path`) and
//! `compiler/rustc_expand/src/mbe/macro_rules.rs` (`get_unused_rule`,
//! `has_compile_error_macro`) (MIT or Apache-2.0).

use hir::InFile;
use hir_def::MacroId;
use hir_expand::name::AsName;
use syntax::{
    AstNode, NodeOrToken, SyntaxKind, SyntaxNode, T, TextRange,
    ast::{self, HasName, HasVisibility},
};

use super::used::Uses;
use crate::{
    cx::Cx,
    lints::{registry::LintId, syntax::Walk},
    lower::{Found, Phase},
    model::Diag,
};

/// rustc's `unused_macros` and `unused_macro_rules`, each at the macro's item.
pub(super) fn lints(cx: &Cx<'_, '_>, walk: &Walk<'_>, uses: &Uses) -> Vec<Found> {
    let mut found = Vec::new();
    let (macros, arms) =
        (cx.lint_may_fire(LintId::named("unused_macros")), cx.lint_may_fire(LintId::named("unused_macro_rules")));
    if !macros && !arms {
        return found;
    }
    for tree in &walk.trees {
        for node in tree.of_kind(|it| matches!(it, SyntaxKind::MACRO_RULES | SyntaxKind::MACRO_DEF)) {
            let Some(def) = ast::Macro::cast(node.clone()) else { continue };
            let Some(name) = def.name() else { continue };
            let text = name.text();
            if text.starts_with('_') || uses.macros_unknown.contains(&name.as_name()) || exported(walk, &def) {
                continue;
            }
            let Some(it) = cx.sema.to_def(&def) else { continue };
            let id = MacroId::from(it);
            let at = tree.ptr(node);
            if !uses.macros.contains(&id) {
                if !macros {
                    continue;
                }
                let span = cx.span(InFile::new(tree.file, name.syntax().text_range()));
                let mut diag = Diag::lint("unused_macros", format!("unused macro definition: `{text}`"), span);
                diag.fact(cx.resolves_fact(it.module(cx.db), text), format!("what the calls of `{text}` resolve to"));
                found.push(Found::lint(diag, Phase::Resolve, at));
                continue;
            }
            if !arms || uses.arms_unknown.contains(&id) {
                continue;
            }
            let matched = uses.arms.get(&id);
            let mut fact = None;
            for (n, rule) in rules(&def).into_iter().enumerate() {
                if rule.compile_error || matched.is_some_and(|it| it.contains(n)) {
                    continue;
                }
                let mut spans = rule.spans.into_iter().map(|it| cx.span(InFile::new(tree.file, it)));
                let Some(first) = spans.next() else { continue };
                let message = format!("rule #{} of macro `{text}` is never used", n + 1);
                let mut diag = Diag::lint("unused_macro_rules", message, first);
                for span in spans {
                    diag.primary(span);
                }
                let fact = fact.get_or_insert_with(|| cx.resolves_fact(it.module(cx.db), text));
                diag.fact(fact.clone(), format!("which rules the calls of `{text}` match"));
                found.push(Found::lint(diag, Phase::Resolve, at));
            }
        }
    }
    found
}

/// Whether `def` is exported, so never counted: a `macro_rules!` with `#[macro_export]`, a
/// `pub macro`.
fn exported(walk: &Walk<'_>, def: &ast::Macro) -> bool {
    match def {
        ast::Macro::MacroRules(it) => walk.attrs(&ast::AnyHasAttrs::new(it.clone())).iter().any(|(meta, _)| {
            let path = match meta {
                ast::Meta::PathMeta(it) => it.path(),
                ast::Meta::TokenTreeMeta(it) => it.path(),
                ast::Meta::KeyValueMeta(it) => it.path(),
                _ => None,
            };
            path.and_then(|it| it.as_single_name_ref()).is_some_and(|it| it.text() == "macro_export")
        }),
        ast::Macro::MacroDef(it) => it.visibility().is_some_and(|it| matches!(it.kind(), ast::VisibilityKind::Pub)),
    }
}

/// A rule of a macro: where rustc points at it (the matcher; an attribute rule's arguments
/// and body), and whether its expansion calls `compile_error!`.
struct Rule {
    spans: Vec<TextRange>,
    compile_error: bool,
}

/// The rules of `def`, in order: `matcher => expansion` separated by `;` in a `macro_rules!`,
/// by `,` in a `macro`, whose `macro m(..) { .. }` form is one rule.
fn rules(def: &ast::Macro) -> Vec<Rule> {
    let body = match def {
        ast::Macro::MacroRules(it) => it.token_tree(),
        ast::Macro::MacroDef(it) => {
            if let Some(args) = it.args() {
                let compile_error = it.body().is_some_and(|it| has_compile_error(it.syntax()));
                return vec![Rule { spans: vec![args.syntax().text_range()], compile_error }];
            }
            it.body()
        }
    };
    let Some(body) = body else { return Vec::new() };
    let elements: Vec<NodeOrToken<SyntaxNode, syntax::SyntaxToken>> = inner(body.syntax());
    let mut rules = Vec::new();
    let mut spans = Vec::new();
    let mut after_arrow = false;
    let mut i = 0;
    while i < elements.len() {
        match &elements[i] {
            NodeOrToken::Token(token) if token.kind() == T![=] => {
                if elements.get(i + 1).is_some_and(|it| it.kind() == T![>]) {
                    i += 1;
                }
                after_arrow = true;
            }
            NodeOrToken::Token(token) if token.kind() == T![=>] => after_arrow = true,
            NodeOrToken::Node(node) if node.kind() == SyntaxKind::TOKEN_TREE => {
                if after_arrow {
                    rules.push(Rule { spans: std::mem::take(&mut spans), compile_error: has_compile_error(node) });
                    after_arrow = false;
                } else {
                    spans.push(node.text_range());
                }
            }
            _ => {}
        }
        i += 1;
    }
    rules
}

/// The elements of a token tree between its delimiters, without trivia.
fn inner(tt: &SyntaxNode) -> Vec<NodeOrToken<SyntaxNode, syntax::SyntaxToken>> {
    let mut elements: Vec<_> = tt.children_with_tokens().filter(|it| !it.kind().is_trivia()).collect();
    let delimiter = |it: Option<&NodeOrToken<SyntaxNode, syntax::SyntaxToken>>| {
        it.is_some_and(|it| matches!(it.kind(), T!['('] | T![')'] | T!['['] | T![']'] | T!['{'] | T!['}']))
    };
    if delimiter(elements.last()) {
        elements.pop();
    }
    if delimiter(elements.first()) {
        elements.remove(0);
    }
    elements
}

/// Whether a token tree has `compile_error!` followed by a delimited group in it, at any
/// depth: rustc's `has_compile_error_macro`.
fn has_compile_error(tt: &SyntaxNode) -> bool {
    // How much of `compile_error ! (..)` the elements so far end with: nothing, the name, the `!`.
    let mut seen = 0u8;
    for element in tt.children_with_tokens() {
        match element {
            NodeOrToken::Token(token) => match token.kind() {
                kind if kind.is_trivia() => {}
                SyntaxKind::IDENT if token.text() == "compile_error" => seen = 1,
                T![!] if seen == 1 => seen = 2,
                _ => seen = 0,
            },
            NodeOrToken::Node(node) => {
                if node.kind() == SyntaxKind::TOKEN_TREE && (seen == 2 || has_compile_error(&node)) {
                    return true;
                }
                seen = 0;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use syntax::{AstNode, SourceFile, ast};

    use super::rules;

    fn spans(text: &str) -> Vec<(Vec<String>, bool)> {
        let file = SourceFile::parse(text, span::Edition::CURRENT).tree();
        let def = file.syntax().descendants().find_map(ast::Macro::cast).unwrap();
        rules(&def)
            .into_iter()
            .map(|rule| (rule.spans.iter().map(|it| text[*it].to_owned()).collect(), rule.compile_error))
            .collect()
    }

    #[test]
    fn rules_of_a_macro_rules_are_its_matchers() {
        let found = spans("macro_rules! m { (a) => {}; [b $x:expr] => { compile_error!(\"no\") }; {} => (()) }");
        assert_eq!(
            found,
            vec![
                (vec!["(a)".to_owned()], false),
                (vec!["[b $x:expr]".to_owned()], true),
                (vec!["{}".to_owned()], false),
            ]
        );
    }

    #[test]
    fn rules_of_a_macro_are_separated_by_commas() {
        let found = spans("macro m { (a) => {}, (b) => { x::compile_error! {} } }");
        assert_eq!(found, vec![(vec!["(a)".to_owned()], false), (vec!["(b)".to_owned()], true)]);
        assert_eq!(spans("macro m($x:expr) { $x }"), vec![(vec!["($x:expr)".to_owned()], false)]);
    }
}
