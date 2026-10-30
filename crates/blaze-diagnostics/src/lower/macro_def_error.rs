//! A `macro_rules!` rustc refuses to define: the checks of rustc's `compile_declarative_macro`
//! (`rustc_expand/src/mbe/macro_rules.rs`), the matcher parsing of `rustc_expand/src/mbe/quoted.rs`
//! and the binder check of `rustc_expand/src/mbe/macro_check.rs` (MIT or Apache-2.0).
//!
//! The checks follow-set ambiguity (`check_matcher`), meta-variable expressions (`${...}`), the
//! `macro_metavar_expr`, `macro_attr` and `macro_derive` feature gates and the
//! `meta_variable_misuse` lint are not ported yet; a definition mbe refuses for one of those is
//! still an error naming what mbe said.

use hir::InFile;
use syntax::{AstNode, NodeOrToken, SyntaxKind, SyntaxToken, TextRange, TextSize, ast};

use super::macro_error::token_descr;
use crate::{
    cx::{Cx, item_range},
    lower::{Found, Phase},
    model::{Applicability, Diag, Span},
};

/// rustc's `VALID_FRAGMENT_NAMES_MSG`.
const VALID_FRAGMENT_NAMES: &str = "valid fragment specifiers are `ident`, `block`, `stmt`, `expr`, `pat`, `ty`, `lifetime`, `literal`, `path`, `meta`, `tt`, `item` and `vis`, along with `expr_2021` and `pat_param` for edition compatibility";

/// rustc's `NonterminalKind::from_symbol`: the same names in every edition.
const FRAGMENTS: &[&str] = &[
    "item", "block", "stmt", "pat", "pat_param", "expr", "expr_2021", "ty", "ident", "lifetime",
    "literal", "meta", "path", "vis", "tt",
];

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::MacroDefError) -> Vec<Found> {
    let node = cx.node(d.node);
    let mut out = check(cx, node.as_ref());
    if out.is_empty() {
        let range = d.name.unwrap_or_else(|| item_range(node.value.syntax()));
        let span = cx.span(InFile::new(d.node.file_id, range));
        let message = format!("blaze can't define this macro: {}", d.message);
        out.push(Found::new(Diag::error(None, message, span), Phase::Resolve));
    }
    out
}

/// rustc's errors on the definition of `mac`, in the order rustc gives them. Empty when rustc
/// defines it.
pub(crate) fn check(cx: &Cx<'_, '_>, mac: InFile<&ast::Macro>) -> Vec<Found> {
    let ast::Macro::MacroRules(rules) = mac.value else { return Vec::new() };
    let Some(body) = rules.token_tree() else { return Vec::new() };
    let mut checker = Checker { cx, file: mac.file_id, errors: Vec::new() };
    checker.definition(&body, item_range(rules.syntax()));
    checker.errors.into_iter().map(|it| Found::new(it, Phase::Resolve)).collect()
}

/// A token tree of rustc's `tokenstream`: a token, or a delimited group.
#[derive(Clone)]
enum Tree {
    Token(SyntaxToken),
    Group(ast::TokenTree),
}

impl Tree {
    fn range(&self) -> TextRange {
        match self {
            Tree::Token(it) => it.text_range(),
            Tree::Group(it) => it.syntax().text_range(),
        }
    }

    fn token(&self) -> Option<&SyntaxToken> {
        match self {
            Tree::Token(it) => Some(it),
            Tree::Group(_) => None,
        }
    }

    fn is(&self, text: &str) -> bool {
        self.token().is_some_and(|it| it.text() == text)
    }
}

/// The trees between `group`'s delimiters, without whitespace and comments other than doc
/// comments, which are tokens to rustc.
fn trees_in(group: &ast::TokenTree) -> Vec<Tree> {
    let open = group.left_delimiter_token();
    let close = group.right_delimiter_token();
    group
        .token_trees_and_tokens()
        .filter_map(|it| match it {
            NodeOrToken::Node(it) => Some(Tree::Group(it)),
            NodeOrToken::Token(it) if Some(&it) == open.as_ref() || Some(&it) == close.as_ref() => None,
            NodeOrToken::Token(it) if it.kind() == SyntaxKind::WHITESPACE => None,
            NodeOrToken::Token(it) if it.kind() == SyntaxKind::COMMENT => {
                ast::Comment::cast(it.clone()).and_then(|c| c.kind().doc).map(|_| Tree::Token(it))
            }
            NodeOrToken::Token(it) => Some(Tree::Token(it)),
        })
        .collect()
}

fn delimiter(group: &ast::TokenTree) -> Option<SyntaxKind> {
    group.left_delimiter_token().map(|it| it.kind())
}

/// rustc's `token.is_ident()`: an identifier, a keyword or `_`.
fn is_ident(token: &SyntaxToken) -> bool {
    token.kind().is_any_identifier() || token.kind() == SyntaxKind::UNDERSCORE
}

/// rustc's `mbe::TokenTree`.
enum Mt {
    Token(TextRange),
    Group(Vec<Mt>),
    Sequence { range: TextRange, tts: Vec<Mt>, separator: bool, op: Kleene, op_range: TextRange },
    Var { range: TextRange, name: String },
    Decl { range: TextRange, name: String, kind: String },
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kleene {
    ZeroOrMore,
    OneOrMore,
    ZeroOrOne,
}

impl Kleene {
    fn of(tree: &Tree) -> Option<Kleene> {
        match tree.token()?.text() {
            "*" => Some(Kleene::ZeroOrMore),
            "+" => Some(Kleene::OneOrMore),
            "?" => Some(Kleene::ZeroOrOne),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Part {
    Pattern,
    Body,
}

struct Checker<'a, 'c, 'db> {
    cx: &'a Cx<'c, 'db>,
    file: hir::HirFileId,
    errors: Vec<Diag>,
}

impl Checker<'_, '_, '_> {
    fn span(&self, range: TextRange) -> Span {
        self.cx.span(InFile::new(self.file, range))
    }

    fn error(&mut self, range: TextRange, message: impl Into<String>) -> &mut Diag {
        let span = self.span(range);
        self.errors.push(Diag::error(None, message, span));
        self.errors.last_mut().expect("just pushed")
    }

    /// rustc's `compile_declarative_macro` over a `macro_rules!` body.
    fn definition(&mut self, body: &ast::TokenTree, item: TextRange) {
        let trees = trees_in(body);
        let mut i = 0;
        let mut rules = 0;
        while i < trees.len() {
            let unsafe_rule = trees[i].is("unsafe");
            let unsafe_range = trees[i].range();
            if unsafe_rule {
                i += 1;
                if self.no_eof(&trees, i, "expected `attr`") {
                    return;
                }
            }
            let mut binders = Vec::new();
            if trees[i].is("attr") {
                i += 1;
                if self.no_eof(&trees, i, "expected macro attr args") {
                    return;
                }
                let args = trees[i].clone();
                i += 1;
                self.args_parens("attr", &args);
                let args = self.parse_one(&args, Part::Pattern);
                self.lhs(&args);
                self.binders(&args, &mut binders);
                if self.no_eof(&trees, i, "expected macro attr body") {
                    return;
                }
            } else if trees[i].is("derive") {
                let derive_range = trees[i].range();
                i += 1;
                if unsafe_rule {
                    self.error(unsafe_range, "`unsafe` is only supported on `attr` rules");
                }
                if self.no_eof(&trees, i, "expected `()` after `derive`") {
                    return;
                }
                let args = trees[i].clone();
                i += 1;
                self.args_parens("derive", &args);
                let args_empty = matches!(&args, Tree::Group(it) if trees_in(it).is_empty());
                if !args_empty {
                    let msg = "`derive` rules do not accept arguments; `derive` must be followed by `()`";
                    self.error(args.range(), msg);
                }
                if self.no_eof(&trees, i, "expected macro derive body") {
                    return;
                }
                if let Some(arrow) = fat_arrow(&trees, i) {
                    let derive = self.span(derive_range);
                    let diag = self.error(arrow, "expected macro derive body, got `=>`");
                    if !args_empty {
                        diag.span_label(derive, "need `()` after this `derive`");
                    }
                    return;
                }
            } else if unsafe_rule {
                self.error(unsafe_range, "`unsafe` is only supported on `attr` rules");
            }
            let lhs = self.parse_one(&trees[i], Part::Pattern);
            let lhs_end = trees[i].range().end();
            i += 1;
            self.lhs(&lhs);
            if fat_arrow(&trees, i).is_none() {
                self.expected(&trees, i, lhs_end, "=>");
                return;
            }
            i += if trees[i].is("=>") { 1 } else { 2 };
            if self.no_eof(&trees, i, "expected right-hand side of macro rule") {
                return;
            }
            let rhs = self.parse_one(&trees[i], Part::Body);
            let rhs_end = trees[i].range().end();
            i += 1;
            if !matches!(rhs, Mt::Group(_)) {
                self.error(trees[i - 1].range(), "macro rhs must be delimited");
            }
            self.binders(&lhs, &mut binders);
            rules += 1;
            if i == trees.len() {
                break;
            }
            if !trees[i].is(";") {
                self.expected(&trees, i, rhs_end, ";");
                return;
            }
            i += 1;
        }
        if rules == 0 {
            self.error(item, "macros must contain at least one rule");
        }
    }

    /// rustc's `check_no_eof`.
    fn no_eof(&mut self, trees: &[Tree], i: usize, label: &str) -> bool {
        if i < trees.len() {
            return false;
        }
        let end = trees.last().map_or_else(|| TextSize::new(0), |it| it.range().end());
        let span = self.span(TextRange::empty(end));
        self.error(TextRange::empty(end), "macro definition ended unexpectedly").span_label(span, label);
        true
    }

    /// rustc's `Parser::expect` of `token` failing at `trees[i]`, `prev_end` being where the
    /// previous token ends.
    fn expected(&mut self, trees: &[Tree], i: usize, prev_end: TextSize, token: &str) {
        let label = format!("expected `{token}`");
        let Some(found) = trees.get(i) else {
            let at = TextRange::empty(prev_end);
            let span = self.span(at);
            self.error(at, format!("expected `{token}`, found end of macro arguments")).span_label(span, label);
            return;
        };
        let found_descr = match found {
            Tree::Token(it) => token_descr(it, self.cx.edition),
            Tree::Group(it) => match it.left_delimiter_token() {
                Some(open) => format!("`{}`", open.text()),
                None => format!("`{}`", it.syntax().text()),
            },
        };
        let at = match found {
            Tree::Token(it) => it.text_range(),
            Tree::Group(it) => it.left_delimiter_token().map_or_else(|| it.syntax().text_range(), |it| it.text_range()),
        };
        let message = format!("expected `{token}`, found {found_descr}");
        let span = self.span(at);
        let between = self.between(prev_end, at.start());
        if token == ";" && between.contains('\n') {
            let prev = self.span(TextRange::empty(prev_end));
            self.error(at, message).span_label(prev, label).span_label(span, "unexpected token");
        } else {
            self.error(at, message).span_label(span, label);
        }
    }

    /// The source text between two offsets of this file.
    fn between(&self, from: TextSize, to: TextSize) -> String {
        let root = self.cx.sema.parse_or_expand(self.file);
        let text = root.text();
        if from >= to || to > text.len() {
            return String::new();
        }
        text.slice(TextRange::new(from, to)).to_string()
    }

    /// rustc's `check_args_parens`.
    fn args_parens(&mut self, rule_kw: &str, args: &Tree) {
        let Tree::Group(group) = args else { return };
        if delimiter(group) == Some(SyntaxKind::L_PAREN) {
            return;
        }
        let (Some(open), Some(close)) = (group.left_delimiter_token(), group.right_delimiter_token()) else { return };
        let parts = vec![(self.span(open.text_range()), "(".to_owned()), (self.span(close.text_range()), ")".to_owned())];
        self.error(group.syntax().text_range(), format!("`{rule_kw}` rule argument matchers require parentheses"))
            .multipart_suggestion("the delimiters should be `(` and `)`", parts, Applicability::MachineApplicable);
    }

    /// rustc's `check_lhs`: delimited, and no repetition that matches nothing.
    fn lhs(&mut self, lhs: &Mt) {
        match lhs {
            Mt::Group(_) => {}
            Mt::Token(range) | Mt::Var { range, .. } | Mt::Decl { range, .. } | Mt::Sequence { range, .. } => {
                self.error(*range, "invalid macro matcher; matchers must be contained in balanced delimiters");
            }
        }
        self.no_empty_sequence(std::slice::from_ref(lhs));
    }

    /// rustc's `check_lhs_no_empty_seq`: the first repetition that can match nothing.
    fn no_empty_sequence(&mut self, tts: &[Mt]) -> bool {
        for tt in tts {
            match tt {
                Mt::Token(_) | Mt::Var { .. } | Mt::Decl { .. } => {}
                Mt::Group(tts) => {
                    if self.no_empty_sequence(tts) {
                        return true;
                    }
                }
                Mt::Sequence { range, tts, separator, op, op_range } => {
                    if !separator && tts.iter().all(matches_empty) {
                        let diag = self.error(*range, "repetition matches empty token tree");
                        if *op == Kleene::ZeroOrOne && matches!(tts.first(), Some(Mt::Decl { kind, .. }) if kind == "vis") {
                            diag.note("a `vis` fragment can already be empty");
                            let open = TextRange::new(range.start() - TextSize::of('$'), range.start() + TextSize::of('('));
                            let close = TextRange::new(range.end() - TextSize::of(')'), op_range.end());
                            let parts = vec![(self.span(open), String::new()), (self.span(close), String::new())];
                            self.errors
                                .last_mut()
                                .expect("just pushed")
                                .multipart_suggestion("remove the `$(` and `)?`", parts, Applicability::MaybeIncorrect);
                        }
                        return true;
                    }
                    if self.no_empty_sequence(tts) {
                        return true;
                    }
                }
            }
        }
        false
    }

    /// The binders of `check_meta_variables`' `check_binders`: a name bound twice in the
    /// matchers of one rule is an error.
    fn binders(&mut self, tt: &Mt, seen: &mut Vec<(String, TextRange)>) {
        match tt {
            Mt::Decl { range, name, .. } => {
                let name = name.strip_prefix("r#").unwrap_or(name);
                if let Some((_, prev)) = seen.iter().find(|(it, _)| it == name) {
                    let (span, prev) = (self.span(*range), self.span(*prev));
                    self.error(*range, "duplicate matcher binding")
                        .span_label(span, "duplicate binding")
                        .span_label(prev, "previous binding");
                } else {
                    seen.push((name.to_owned(), *range));
                }
            }
            Mt::Group(tts) | Mt::Sequence { tts, .. } => tts.iter().for_each(|it| self.binders(it, seen)),
            Mt::Token(_) | Mt::Var { .. } => {}
        }
    }

    /// rustc's `parse_one_tt`.
    fn parse_one(&mut self, tree: &Tree, part: Part) -> Mt {
        let mut tts = self.parse(std::slice::from_ref(tree), part);
        tts.pop().unwrap_or(Mt::Token(tree.range()))
    }

    /// rustc's `quoted::parse`.
    fn parse(&mut self, trees: &[Tree], part: Part) -> Vec<Mt> {
        let mut result = Vec::new();
        let mut i = 0;
        while i < trees.len() {
            let tree = self.parse_tree(trees, &mut i, part);
            let Mt::Var { range: start, name } = tree else {
                result.push(tree);
                continue;
            };
            if part == Part::Body {
                result.push(Mt::Var { range: start, name });
                continue;
            }
            let decl = |range, kind: &str| Mt::Decl { range, name: name.clone(), kind: kind.to_owned() };
            let Some(colon) = trees.get(i).filter(|it| it.is(":")) else {
                result.push(decl(start, "tt"));
                self.missing_fragment(start, TextRange::empty(start.end()));
                continue;
            };
            let colon = colon.range();
            i += 1;
            let next = trees.get(i).cloned();
            if next.is_some() {
                i += 1;
            }
            let Some(Tree::Token(token)) = next else {
                result.push(decl(TextRange::new(start.start(), colon.end()), "tt"));
                self.missing_fragment(TextRange::new(start.start(), colon.end()), TextRange::empty(colon.end()));
                continue;
            };
            if !is_ident(&token) {
                if token.kind() == SyntaxKind::DOLLAR
                    && trees.get(i).and_then(Tree::token).is_some_and(is_ident)
                {
                    let span = self.span(token.text_range());
                    self.error(token.text_range(), "missing fragment specifier")
                        .note("fragment specifiers must be provided")
                        .help(VALID_FRAGMENT_NAMES)
                        .suggestion_verbose(
                            span,
                            "fragment specifiers should not be prefixed with `$`",
                            "",
                            Applicability::MaybeIncorrect,
                        );
                    result.push(decl(token.text_range(), "tt"));
                } else {
                    result.push(decl(token.text_range(), "tt"));
                    self.missing_fragment(token.text_range(), TextRange::empty(token.text_range().end()));
                }
                continue;
            }
            let range = TextRange::new(start.start(), token.text_range().end());
            let fragment = token.text();
            let unraw = fragment.strip_prefix("r#").unwrap_or(fragment);
            if FRAGMENTS.contains(&unraw) {
                result.push(decl(range, unraw));
            } else {
                self.error(range, format!("invalid fragment specifier `{fragment}`")).help(VALID_FRAGMENT_NAMES);
                result.push(decl(range, "tt"));
            }
        }
        result
    }

    /// rustc's `MissingFragmentSpecifier`.
    fn missing_fragment(&mut self, range: TextRange, add: TextRange) {
        let add = self.span(add);
        self.error(range, "missing fragment specifier")
            .note("fragment specifiers must be provided")
            .help(VALID_FRAGMENT_NAMES)
            .suggestion_verbose(add, "try adding a specifier here", ":spec", Applicability::MaybeIncorrect);
    }

    /// rustc's `quoted::parse_tree`.
    fn parse_tree(&mut self, trees: &[Tree], i: &mut usize, part: Part) -> Mt {
        let tree = trees[*i].clone();
        *i += 1;
        let dollar = match &tree {
            Tree::Token(it) if it.kind() == SyntaxKind::DOLLAR => it.text_range(),
            Tree::Token(it) => return Mt::Token(it.text_range()),
            Tree::Group(group) => return Mt::Group(self.parse(&trees_in(group), part)),
        };
        let Some(next) = trees.get(*i).cloned() else { return Mt::Token(dollar) };
        *i += 1;
        match next {
            Tree::Group(group) => {
                let delim = delimiter(&group);
                let entire = group.syntax().text_range();
                match part {
                    Part::Pattern if delim != Some(SyntaxKind::L_PAREN) => {
                        let open = group.left_delimiter_token().map_or_else(String::new, |it| it.text().to_owned());
                        self.dollar_in_lhs(entire, &open);
                    }
                    Part::Body if delim == Some(SyntaxKind::L_BRACK) => {
                        self.error(entire, "expected `(` or `{`, found `[`");
                    }
                    _ => {}
                }
                let tts = self.parse(&trees_in(&group), part);
                let (separator, op, op_range) = self.separator_and_kleene(trees, i, entire);
                Mt::Sequence { range: entire, tts, separator, op, op_range }
            }
            Tree::Token(token) if is_ident(&token) => {
                let range = TextRange::new(dollar.start(), token.text_range().end());
                if token.text() == "crate" {
                    Mt::Token(range)
                } else {
                    Mt::Var { range, name: token.text().to_owned() }
                }
            }
            Tree::Token(token) if token.kind() == SyntaxKind::DOLLAR => {
                if part == Part::Pattern {
                    self.dollar_in_lhs(token.text_range(), "$");
                }
                Mt::Token(token.text_range())
            }
            Tree::Token(token) => {
                self.error(token.text_range(), format!("expected identifier, found `{}`", token.text()));
                Mt::Var { range: token.text_range(), name: String::new() }
            }
        }
    }

    /// rustc's `span_dollar_dollar_or_metavar_in_the_lhs_err`.
    fn dollar_in_lhs(&mut self, range: TextRange, token: &str) {
        let span = self.span(range);
        self.error(range, format!("unexpected token: {token}")).span_note(
            span,
            "`$$` and meta-variable expressions are not allowed inside macro parameter definitions",
        );
    }

    /// rustc's `parse_sep_and_kleene_op`.
    fn separator_and_kleene(&mut self, trees: &[Tree], i: &mut usize, span: TextRange) -> (bool, Kleene, TextRange) {
        let first = trees.get(*i).cloned();
        if first.is_some() {
            *i += 1;
        }
        let at = match first {
            Some(tree) if Kleene::of(&tree).is_some() => {
                return (false, Kleene::of(&tree).expect("checked"), tree.range());
            }
            Some(Tree::Token(separator)) => {
                let second = trees.get(*i).cloned();
                if second.is_some() {
                    *i += 1;
                }
                match second {
                    Some(tree) if Kleene::of(&tree) == Some(Kleene::ZeroOrOne) => {
                        self.error(separator.text_range(), "the `?` macro repetition operator does not take a separator");
                        return (false, Kleene::ZeroOrMore, tree.range());
                    }
                    Some(tree) if Kleene::of(&tree).is_some() => {
                        return (true, Kleene::of(&tree).expect("checked"), tree.range());
                    }
                    Some(tree) => tree.range(),
                    None => separator.text_range(),
                }
            }
            Some(group) => group.range(),
            None => span,
        };
        self.error(at, "expected one of: `*`, `+`, or `?`");
        (false, Kleene::ZeroOrMore, at)
    }
}

/// rustc's `is_empty_token_tree` for one tree of a repetition.
fn matches_empty(tt: &Mt) -> bool {
    match tt {
        Mt::Decl { kind, .. } => kind == "vis",
        Mt::Sequence { op, .. } => matches!(op, Kleene::ZeroOrMore | Kleene::ZeroOrOne),
        Mt::Token(_) | Mt::Group(_) | Mt::Var { .. } => false,
    }
}

/// The range of a `=>` at `trees[i]`, which the lexer may give as one token or as `=` and `>`.
fn fat_arrow(trees: &[Tree], i: usize) -> Option<TextRange> {
    let first = trees.get(i)?;
    if first.is("=>") {
        return Some(first.range());
    }
    let second = trees.get(i + 1)?;
    (first.is("=") && second.is(">") && first.range().end() == second.range().start())
        .then(|| TextRange::new(first.range().start(), second.range().end()))
}
