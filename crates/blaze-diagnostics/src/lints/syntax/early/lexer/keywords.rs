//! `keyword_idents_2018` and `keyword_idents_2024` (the group `keyword_idents`): an identifier
//! that a later edition makes a keyword. rustc's pre-expansion pass gives them, on each file as
//! parsed, before `cfg` removes anything and before any macro is expanded: every identifier
//! its AST visitor comes to (`check_ident`), and every token of a macro call's arguments and of
//! a macro definition's body (`check_mac`, `check_mac_def`), but none of an attribute's
//! delimited arguments, which the visitor does not walk.
//!
//! Follows rustc's `compiler/rustc_lint/src/builtin.rs` (`KeywordIdents`: `check_tokens`,
//! `check_ident_token`), `compiler/rustc_lint/src/lints.rs` (`BuiltinKeywordIdents`),
//! `compiler/rustc_lint/src/lib.rs` (`BuiltinCombinedPreExpansionLintPass`, the
//! `keyword_idents` group), `compiler/rustc_ast/src/visit.rs` (what the visitor walks of an
//! attribute) and `compiler/rustc_interface/src/passes.rs` (`pre_expansion_lint`, of the crate
//! root and of each module file `rustc_expand/src/expand.rs` loads) (MIT or Apache-2.0).

use hir::InFile;
use span::Edition;
use syntax::{
    AstNode, NodeOrToken, SyntaxKind, SyntaxNode, SyntaxNodePtr, SyntaxToken, TextRange, TextSize, WalkEvent,
    ast,
};

use crate::{
    cx::Cx,
    lints::syntax::{Emitted, Pass, Tree, source_fact},
    lower::Phase,
    model::{Applicability, Diag},
};

/// The lints of `tree`, a file of the crate, into `out`.
pub(super) fn lints(cx: &Cx<'_, '_>, tree: &Tree, out: &mut Vec<Emitted>) {
    let edition = tree.file.edition(cx.db);
    if edition.at_least_2024() {
        return;
    }
    let mut keywords = Keywords { cx, tree, edition, out };
    keywords.walk(&tree.root);
}

struct Keywords<'c, 'a, 'db> {
    cx: &'c Cx<'a, 'db>,
    tree: &'c Tree,
    edition: Edition,
    out: &'c mut Vec<Emitted>,
}

/// Whether an identifier is a token of a macro's (`UnderMacro`): `dyn` is linted only outside.
#[derive(Clone, Copy, PartialEq, Eq)]
enum UnderMacro {
    No,
    Yes,
}

impl Keywords<'_, '_, '_> {
    /// The identifiers of the AST `node`, as rustc's visitor comes to them.
    fn walk(&mut self, node: &SyntaxNode) {
        let mut events = node.preorder_with_tokens();
        while let Some(event) = events.next() {
            let WalkEvent::Enter(element) = event else { continue };
            match element {
                NodeOrToken::Node(node) => match node.kind() {
                    SyntaxKind::ATTR => {
                        if let Some(meta) = ast::Attr::cast(node).and_then(|it| it.meta()) {
                            self.meta(meta);
                        }
                        events.skip_subtree();
                    }
                    SyntaxKind::TOKEN_TREE => {
                        if let Some(owner) = node.parent()
                            && matches!(owner.kind(), SyntaxKind::MACRO_CALL | SyntaxKind::MACRO_RULES | SyntaxKind::MACRO_DEF)
                        {
                            self.tokens(&node, &owner);
                        }
                        events.skip_subtree();
                    }
                    _ => {}
                },
                NodeOrToken::Token(token) => {
                    if let Some(parent) = token.parent() {
                        self.token(&token, UnderMacro::No, &parent);
                    }
                }
            }
        }
    }

    /// What the visitor walks of an attribute: its path, and the expression of `path = expr`.
    fn meta(&mut self, meta: ast::Meta) {
        match meta {
            ast::Meta::PathMeta(it) => self.walk_opt(it.path().map(|it| it.syntax().clone())),
            ast::Meta::TokenTreeMeta(it) => self.walk_opt(it.path().map(|it| it.syntax().clone())),
            ast::Meta::KeyValueMeta(it) => {
                self.walk_opt(it.path().map(|it| it.syntax().clone()));
                self.walk_opt(it.expr().map(|it| it.syntax().clone()));
            }
            ast::Meta::UnsafeMeta(it) => {
                if let Some(inner) = it.meta() {
                    self.meta(inner);
                }
            }
            ast::Meta::CfgMeta(_) | ast::Meta::CfgAttrMeta(_) => {}
        }
    }

    fn walk_opt(&mut self, node: Option<SyntaxNode>) {
        if let Some(node) = node {
            self.walk(&node);
        }
    }

    /// `check_tokens` of the token tree `tt` of the macro call or definition `owner`: each
    /// identifier and lifetime not after a `$`.
    fn tokens(&mut self, tt: &SyntaxNode, owner: &SyntaxNode) {
        let mut prev_dollar = false;
        for token in tt.descendants_with_tokens().filter_map(|it| it.into_token()) {
            if token.kind().is_trivia() {
                continue;
            }
            if token.kind() == SyntaxKind::DOLLAR {
                prev_dollar = true;
                continue;
            }
            if !prev_dollar {
                self.token(&token, UnderMacro::Yes, owner);
            }
            prev_dollar = false;
        }
    }

    /// `check_ident_token` of `token` if it is an identifier or a lifetime, the lint decided
    /// at `at`.
    fn token(&mut self, token: &SyntaxToken, under_macro: UnderMacro, at: &SyntaxNode) {
        let text = token.text();
        let range = token.text_range();
        match token.kind() {
            SyntaxKind::IDENT if !text.starts_with("r#") => self.check(text, range, "", under_macro, at),
            SyntaxKind::LIFETIME_IDENT => {
                let Some(name) = text.strip_prefix('\'') else { return };
                match name.strip_prefix("r#") {
                    // Before 2021 rustc lexes `'r#name` as `'r`, `#` and the identifier `name`.
                    Some(rest) if !self.edition.at_least_2021() => {
                        let range = TextRange::new(range.start() + TextSize::from(3), range.end());
                        self.check(rest, range, "", under_macro, at);
                    }
                    Some(_) => {}
                    None => self.check(name, range, "'", under_macro, at),
                }
            }
            _ => {}
        }
    }

    /// `check_ident_token` of the identifier `name` at `range` (a lifetime's when `prefix` is
    /// `'`, its span the lifetime's).
    fn check(&mut self, name: &str, range: TextRange, prefix: &str, under_macro: UnderMacro, at: &SyntaxNode) {
        let (lint, next, year) = match name {
            "async" | "await" | "try" => ("keyword_idents_2018", Edition::Edition2018, "2018"),
            "dyn" if under_macro == UnderMacro::No => ("keyword_idents_2018", Edition::Edition2018, "2018"),
            "gen" => ("keyword_idents_2024", Edition::Edition2024, "2024"),
            _ => return,
        };
        if self.edition >= next {
            return;
        }
        let cx = self.cx;
        let span = cx.span(InFile::new(self.tree.file, range));
        let mut diag = Diag::lint(lint, format!("`{name}` is a keyword in the {year} edition"), span.clone());
        diag.suggestion(
            span,
            "you can use a raw identifier to stay compatible",
            format!("{prefix}r#{name}"),
            Applicability::MachineApplicable,
        );
        let at: InFile<SyntaxNodePtr> = self.tree.ptr(at);
        source_fact(cx, &mut diag, at);
        self.out.push(Emitted::new(diag, Phase::Resolve, Pass::PreExpansion, at));
    }
}
