//! Lexing, bridging to parser (which does the actual parsing) and
//! incremental reparsing.

mod reparsing;

use std::sync::Arc;

use parser::{Edition, OutlineStrStep, StrStep};
use syntax_tree::{FlatTree, TextRange, TextSize};

use crate::{SyntaxError, SyntaxKind, SyntaxTreeBuilder, Tree, syntax_node::RustLanguage};

pub(crate) use crate::parsing::reparsing::{Reparsed, incremental_reparse};

/// A source file's tree: in full, or as an outline whose `fn` bodies are lazy nodes.
pub(crate) fn parse_text(
    text: Arc<str>,
    edition: Edition,
    outline: bool,
) -> (Tree, Vec<SyntaxError>) {
    let _p = tracing::info_span!("parse_text", outline).entered();
    let lexed = parser::LexedStr::new(edition, &text);
    let parser_input = lexed.to_input(edition);
    let parser_output = match outline {
        true => parser::TopEntryPoint::SourceFile.parse_outline(&parser_input),
        false => parser::TopEntryPoint::SourceFile.parse(&parser_input),
    };
    let (tree, errors, _eof) = build_tree(text.clone(), &lexed, &parser_output, edition);
    (Tree::new(Arc::new(tree)), errors)
}

pub(crate) fn parse_text_at(
    text: Arc<str>,
    entry: parser::TopEntryPoint,
    edition: Edition,
) -> (Tree, Vec<SyntaxError>) {
    let _p = tracing::info_span!("parse_text_at").entered();
    let lexed = parser::LexedStr::new(edition, &text);
    let parser_input = lexed.to_input(edition);
    let parser_output = entry.parse(&parser_input);
    let (tree, errors, _eof) = build_tree(text.clone(), &lexed, &parser_output, edition);
    (Tree::new(Arc::new(tree)), errors)
}

/// The tree of `lexed`, the lexing of `source`, from the parser's output (full or outline),
/// its parser errors then its lexer errors, and whether every token was taken.
pub(crate) fn build_tree(
    source: Arc<str>,
    lexed: &parser::LexedStr<'_>,
    parser_output: &parser::Output,
    edition: Edition,
) -> (FlatTree, Vec<SyntaxError>, bool) {
    let _p = tracing::info_span!("build_tree").entered();
    debug_assert_eq!(lexed.as_str(), &*source);
    let mut builder = SyntaxTreeBuilder::sourced(source);

    let is_eof = lexed.intersperse_trivia_outline(parser_output, &mut |step| match step {
        OutlineStrStep::Step(StrStep::Token { kind, text }) => builder.token(kind, text),
        OutlineStrStep::Step(StrStep::Enter { kind }) => builder.start_node(kind),
        OutlineStrStep::Step(StrStep::Exit) => builder.finish_node(),
        OutlineStrStep::Step(StrStep::Error { msg, pos }) => {
            builder.error(msg.to_owned(), pos.try_into().unwrap())
        }
        OutlineStrStep::Lazy { kind, text } => builder.lazy(kind, TextSize::of(text)),
    });

    let (tree, mut errors) = builder.finish_raw(edition);
    errors.extend(lexer_errors(lexed, TextSize::new(0)));
    (tree, errors, is_eof)
}

/// The errors of `lexed`'s tokens, their ranges moved by `offset`.
pub(crate) fn lexer_errors<'a>(
    lexed: &'a parser::LexedStr<'_>,
    offset: TextSize,
) -> impl Iterator<Item = SyntaxError> + 'a {
    lexed.errors().map(move |(i, err)| {
        let text_range = lexed.text_range(i);
        let text_range = TextRange::new(
            text_range.start.try_into().unwrap(),
            text_range.end.try_into().unwrap(),
        );
        SyntaxError::new(err, text_range + offset)
    })
}

/// What parses an outline's lazy node: the body block of a `fn`, its text parsed alone as a
/// block in the file's edition (`data`), which is exactly the subtree a full parse of the
/// file makes there (`parser`'s outline module says why). The errors of that parse are not
/// kept here: [`body_parser_errors`] finds them when asked.
pub(crate) fn expand_lazy(kind: syntax_tree::SyntaxKind, text: &str, data: u32) -> FlatTree {
    let kind = <RustLanguage as syntax_tree::Language>::kind_from_raw(kind);
    let edition = Edition::from_u32(data);
    let reparser = parser::Reparser::for_node(kind, None, None)
        .unwrap_or_else(|| panic!("a lazy {kind:?}: only blocks are left unparsed"));
    let lexed = parser::LexedStr::new(edition, text);
    let output = reparser.parse(&lexed.to_input(edition));
    let (tree, _errors, is_eof) = build_tree(Arc::from(text), &lexed, &output, edition);
    assert!(is_eof, "a lazy {kind:?} of {} bytes parsed short of its end", text.len());
    tree
}

/// The parser's errors in the lazy node of `kind` at `range` of `source`, its file: the errors
/// a full parse of the file reports there. Its lexer errors are the outline's.
pub(crate) fn body_parser_errors(
    kind: SyntaxKind,
    source: &str,
    range: TextRange,
    edition: Edition,
) -> Vec<SyntaxError> {
    let Some(reparser) = parser::Reparser::for_node(kind, None, None) else {
        panic!("a lazy {kind:?}: only blocks are left unparsed")
    };
    let lexed = parser::LexedStr::new(edition, &source[range]);
    let output = reparser.parse(&lexed.to_input(edition));
    let mut errors = Vec::new();
    lexed.intersperse_trivia(&output, &mut |step| {
        if let StrStep::Error { msg, pos } = step {
            errors.push((msg.to_owned(), pos));
        }
    });
    let mut tokens_before = None;
    errors
        .into_iter()
        .map(|(msg, pos)| {
            // This one error's position is the number of its token in the file, not an offset.
            let pos = match msg == ILLEGAL_FLOAT {
                true => {
                    *tokens_before.get_or_insert_with(|| {
                        parser::LexedStr::new(edition, &source[..usize::from(range.start())]).len()
                    }) + pos
                }
                false => usize::from(range.start()) + pos,
            };
            SyntaxError::new_at_offset(msg, TextSize::try_from(pos).unwrap())
        })
        .collect()
}

/// The message `LexedStr::intersperse_trivia` reports at a float split it cannot make.
const ILLEGAL_FLOAT: &str = "illegal float literal";
