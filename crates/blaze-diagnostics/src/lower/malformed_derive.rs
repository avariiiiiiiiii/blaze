//! A `#[derive]` that is not a list: "malformed `derive` attribute input" with the form it
//! must have, or "wrong meta list delimiters" for one in `[]` or `{}`; and the errors of the
//! elements of a list, which hir passes over: E0777 for a literal, and a path with arguments
//! or a value.
//!
//! Follows rustc's `compiler/rustc_builtin_macros/src/derive.rs` (`Expander::expand`,
//! `report_unexpected_meta_item_lit`, `report_path_args`), `diagnostics.rs` (`BadDeriveLit`,
//! `DerivePathArgsList`, `DerivePathArgsValue`) and `compiler/rustc_attr_parsing/src/
//! validate_attr.rs` (`emit_malformed_attribute`, `check_meta_bad_delim`) (MIT or Apache-2.0).

use hir::InFile;
use syntax::{AstNode, NodeOrToken, SyntaxElement, SyntaxKind, T, algo::find_node_at_range, ast};

use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::{Applicability, Diag, Span, SuggestionStyle},
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::MalformedDerive) -> Vec<Found> {
    let file = d.range.file_id;
    let root = cx.sema.parse_or_expand(file);
    let Some(attr) = find_node_at_range::<ast::Attr>(&root, d.range.value) else {
        return vec![malformed(cx.span(d.range), "")];
    };
    let inner = if attr.excl_token().is_some() { "!" } else { "" };
    let meta = attr
        .skip_cfg_attrs()
        .into_iter()
        .find(|it| it.simple_name().as_deref() == Some("derive"))
        .map(|it| match it {
            ast::Meta::UnsafeMeta(unsafe_meta) => unsafe_meta.meta().unwrap_or(ast::Meta::UnsafeMeta(unsafe_meta)),
            it => it,
        });
    let Some(meta) = meta else { return vec![malformed(cx.span(d.range), inner)] };
    // An attribute of a `cfg_attr` is spanned by its own meta item once rustc expands it.
    let span = match meta.syntax().parent().is_some_and(|it| ast::Attr::can_cast(it.kind())) {
        true => cx.span(InFile::new(file, attr.syntax().text_range())),
        false => cx.span(InFile::new(file, meta.syntax().text_range())),
    };
    if let ast::Meta::TokenTreeMeta(list) = &meta
        && let Some(tt) = list.token_tree()
        && tt.l_paren_token().is_none()
        && let (Some(open), Some(close)) = (tt.left_delimiter_token(), tt.right_delimiter_token())
    {
        let mut diag = Diag::error(
            None,
            "wrong meta list delimiters",
            cx.span(InFile::new(file, tt.syntax().text_range())),
        );
        diag.multipart_suggestion(
            "the delimiters should be `(` and `)`",
            vec![
                (cx.span(InFile::new(file, open.text_range())), "(".to_owned()),
                (cx.span(InFile::new(file, close.text_range())), ")".to_owned()),
            ],
            Applicability::MachineApplicable,
        );
        return vec![Found::new(diag, Phase::Resolve)];
    }
    vec![malformed(span, inner)]
}

/// rustc's `emit_malformed_attribute` of `derive`, whose one form is a list.
fn malformed(span: Span, inner: &str) -> Found {
    let mut diag = Diag::error(None, "malformed `derive` attribute input", span.clone());
    diag.suggestions(
        span,
        "must be of the form",
        [format!("#{inner}[derive(Trait1, Trait2, ...)]")],
        Applicability::HasPlaceholders,
        SuggestionStyle::ShowAlways,
    );
    Found::new(diag, Phase::Resolve)
}

/// The errors rustc gives of the elements of the list `tt` of a `#[derive(...)]` that is in
/// effect: a literal (E0777), and a path with arguments or a value. hir drops such an element
/// and reports nothing, so the attribute checks call this for each derive of an item.
pub(crate) fn elements(cx: &Cx<'_, '_>, tt: InFile<ast::TokenTree>) -> Vec<Found> {
    let file = tt.file_id;
    let span = |range| cx.span(InFile::new(file, range));
    let mut found = Vec::new();
    for element in list_elements(&tt.value) {
        let [first, rest @ ..] = element.as_slice() else { continue };
        if let NodeOrToken::Token(lit) = first
            && is_literal(lit.kind())
        {
            let at = span(lit.text_range());
            let mut diag = Diag::error(Some("E0777"), "expected path to a trait, found literal", at.clone());
            diag.span_label(at, "not a trait");
            match cooked_ident(lit.kind(), lit.text()) {
                Some(name) => diag.help(format!("try using `#[derive({name})]`")),
                None => diag.help("for example, write `#[derive(Debug)]` for `Debug`"),
            };
            found.push(Found::new(diag, Phase::Resolve));
            continue;
        }
        let path_len = rest.iter().take_while(|it| is_path_part(it)).count() + 1;
        let Some(after) = element.get(path_len) else { continue };
        let path_end = element[path_len - 1].text_range().end();
        let end = element.last().map_or(path_end, |it| it.text_range().end());
        let at = span(syntax::TextRange::new(path_end, end));
        let (message, help) = match after {
            NodeOrToken::Node(node) if node.kind() == SyntaxKind::TOKEN_TREE => {
                ("traits in `#[derive(...)]` don't accept arguments", "remove the arguments")
            }
            NodeOrToken::Token(token) if token.kind() == T![=] => {
                ("traits in `#[derive(...)]` don't accept values", "remove the value")
            }
            _ => continue,
        };
        let mut diag = Diag::error(None, message, at.clone());
        diag.suggestion(at, help, "", Applicability::MachineApplicable);
        found.push(Found::new(diag, Phase::Resolve));
    }
    found
}

/// The elements of a delimited token tree between its commas, each without trivia.
fn list_elements(tt: &ast::TokenTree) -> Vec<Vec<SyntaxElement>> {
    let children: Vec<SyntaxElement> = tt.syntax().children_with_tokens().collect();
    let inside = match children.as_slice() {
        [_, inside @ .., _] => inside,
        _ => &[],
    };
    let mut elements = vec![Vec::new()];
    for child in inside {
        match child {
            NodeOrToken::Token(token) if token.kind() == T![,] => elements.push(Vec::new()),
            NodeOrToken::Token(token) if token.kind().is_trivia() => {}
            it => elements.last_mut().expect("one element at least").push(it.clone()),
        }
    }
    elements.retain(|it| !it.is_empty());
    elements
}

fn is_literal(kind: SyntaxKind) -> bool {
    kind.is_literal() || matches!(kind, T![true] | T![false])
}

fn is_path_part(element: &SyntaxElement) -> bool {
    match element {
        NodeOrToken::Token(token) => {
            matches!(token.kind(), SyntaxKind::IDENT | T![::] | T![self] | T![super] | T![crate] | T![Self])
        }
        NodeOrToken::Node(_) => false,
    }
}

/// The name in a cooked string literal when it is an identifier (`rustc_lexer::is_ident`), as
/// rustc's `BadDeriveLitHelp::StrLit` names it.
fn cooked_ident(kind: SyntaxKind, text: &str) -> Option<&str> {
    if kind != SyntaxKind::STRING {
        return None;
    }
    let name = text.strip_prefix('"')?.strip_suffix('"')?;
    let mut chars = name.chars();
    let first = chars.next()?;
    let starts = first == '_' || unicode_ident::is_xid_start(first);
    (starts && chars.all(unicode_ident::is_xid_continue)).then_some(name)
}
