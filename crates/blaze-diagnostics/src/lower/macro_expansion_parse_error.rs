//! A macro's expansion that does not parse as the fragment its call stands for. rustc's parser
//! stops at the first error of a fragment (`rustc_expand/src/expand.rs`'s `parse_ast_fragment`,
//! MIT or Apache-2.0), so only the first is given, at its place in the expansion.
//!
//! The parser's own text is rust-analyzer's; rustc's `expected {thing}, found {token}` is given
//! for an expression, type or pattern, the rest as the parser says it.

use hir::InFile;
use syntax::{AstNode, NodeOrToken, SyntaxNode, TextRange, ast};

use super::macro_error::token_descr;
use crate::{
    cx::Cx,
    lower::{Found, Phase},
    model::Diag,
};

pub(crate) fn lower(cx: &Cx<'_, '_>, d: &hir::MacroExpansionParseError) -> Vec<Found> {
    let Some(error) = d.errors.first() else { return Vec::new() };
    let message = error.to_string();
    let root = cx.sema.parse_or_expand(d.range.file_id);
    let expansion = match root.covering_element(d.range.value) {
        NodeOrToken::Node(it) => Some(it),
        NodeOrToken::Token(it) => it.parent(),
    }
    .and_then(|it| it.ancestors().find_map(ast::MacroCall::cast))
    .and_then(|call| cx.sema.expand_macro_call(&call));
    let Some(expansion) = expansion else {
        let message = format!("the expansion of this macro does not parse: {message}");
        return vec![Found::new(Diag::error(None, message, cx.span(d.range)), Phase::Resolve)];
    };
    let range = error.range();
    let message = match found(cx, &expansion.value, range) {
        Some(found) if is_fragment(&message) => format!("{message}, found {found}"),
        _ => message,
    };
    let span = cx.span(InFile::new(expansion.file_id, range));
    vec![Found::new(Diag::error(None, message, span), Phase::Resolve)]
}

fn is_fragment(message: &str) -> bool {
    matches!(message, "expected expression" | "expected type" | "expected pattern")
}

/// rustc's `token_descr` of the token the parser stopped at.
fn found(cx: &Cx<'_, '_>, root: &SyntaxNode, range: TextRange) -> Option<String> {
    let token = root
        .descendants_with_tokens()
        .filter_map(NodeOrToken::into_token)
        .find(|it| !it.kind().is_trivia() && it.text_range().start() >= range.start())?;
    Some(token_descr(&token, cx.edition))
}
