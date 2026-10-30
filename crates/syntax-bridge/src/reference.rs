//! The conversions the build no longer runs, kept as the references tests hold the new ones to
//! (compiled for this crate's tests and with the `reference` feature, which only tests turn on):
//! the parser's input made by a cursor over the token trees, each leaf decoded, identifiers'
//! kinds from their text and editions through a map.

use parser::TopEntryPoint;
use rustc_hash::FxHashMap;
use span::{Edition, SpanMap, SyntaxContext};
use syntax::{Parse, SyntaxKind, SyntaxKind::*, SyntaxNode, T};

/// [`crate::to_parser_input`] as it was.
pub fn to_parser_input(
    buffer: tt::TokenTreesView<'_>,
    // dyn: a reference for tests, never on a build's path.
    span_to_edition: &mut dyn FnMut(SyntaxContext) -> Edition,
) -> parser::Input {
    convert(buffer, span_to_edition, false)
}

/// [`crate::to_matcher_input`] as it was.
pub fn to_matcher_input(
    buffer: tt::TokenTreesView<'_>,
    // dyn: a reference for tests, never on a build's path.
    span_to_edition: &mut dyn FnMut(SyntaxContext) -> Edition,
) -> parser::Input {
    convert(buffer, span_to_edition, true)
}

/// [`crate::token_tree_to_syntax_node`] parsing the input [`to_parser_input`] makes.
pub fn token_tree_to_syntax_node(
    tt: &tt::TopSubtree,
    entry_point: TopEntryPoint,
    // dyn: a reference for tests, never on a build's path.
    span_to_edition: &mut dyn FnMut(SyntaxContext) -> Edition,
) -> (Parse<SyntaxNode>, SpanMap) {
    let buffer = tt.view().strip_invisible();
    let parser_input = to_parser_input(buffer, span_to_edition);
    crate::parse_tokens(buffer, entry_point, &parser_input)
}

/// Whether two inputs are the same tokens, each the same to the parser.
pub fn same_input(a: &parser::Input, b: &parser::Input) -> bool {
    a.len() == b.len() && (0..a.len()).all(|i| a.token(i) == b.token(i))
}

fn convert(
    buffer: tt::TokenTreesView<'_>,
    // dyn: a reference for tests, never on a build's path.
    span_to_edition: &mut dyn FnMut(SyntaxContext) -> Edition,
    fragments: bool,
) -> parser::Input {
    let mut res = parser::Input::with_capacity(buffer.len());
    // Where the fragment that was opened last starts, until its first token is pushed.
    let mut fragment_at = None;

    let mut current = buffer.cursor();
    let mut syntax_context_to_edition_cache = FxHashMap::default();
    let mut last: Option<(SyntaxContext, Edition)> = None;
    let mut ctx_edition = |ctx: SyntaxContext| {
        if let Some((last_ctx, edition)) = last
            && last_ctx == ctx
        {
            return edition;
        }
        let edition =
            *syntax_context_to_edition_cache.entry(ctx).or_insert_with(|| span_to_edition(ctx));
        last = Some((ctx, edition));
        edition
    };

    while !current.eof() {
        if let Some(at) = fragment_at
            && res.len() > at
        {
            res.mark_fragment_start(at);
            fragment_at = None;
        }
        let tt = current.token_tree();

        // Check if it is lifetime
        if let Some(tt::TokenTree::Leaf(tt::Leaf::Punct(punct))) = tt
            && punct.char == '\''
        {
            current.bump();
            match current.token_tree() {
                Some(tt::TokenTree::Leaf(tt::Leaf::Ident(ident))) => {
                    res.push(LIFETIME_IDENT, ctx_edition(ident.span.ctx));
                    current.bump();
                    continue;
                }
                _ => panic!("Next token must be ident"),
            }
        }

        match tt {
            Some(tt::TokenTree::Leaf(leaf)) => {
                match leaf {
                    tt::Leaf::Literal(lit) => {
                        let kind = match lit.kind {
                            tt::LitKind::Byte => SyntaxKind::BYTE,
                            tt::LitKind::Char => SyntaxKind::CHAR,
                            tt::LitKind::Integer => SyntaxKind::INT_NUMBER,
                            tt::LitKind::Float => SyntaxKind::FLOAT_NUMBER,
                            tt::LitKind::Str | tt::LitKind::StrRaw(_) => SyntaxKind::STRING,
                            tt::LitKind::ByteStr | tt::LitKind::ByteStrRaw(_) => {
                                SyntaxKind::BYTE_STRING
                            }
                            tt::LitKind::CStr | tt::LitKind::CStrRaw(_) => SyntaxKind::C_STRING,
                            tt::LitKind::Err(_) => SyntaxKind::ERROR,
                        };
                        res.push(kind, ctx_edition(lit.span.ctx));

                        if kind == FLOAT_NUMBER && !lit.text().ends_with('.') {
                            res.was_joint();
                        }
                    }
                    tt::Leaf::Ident(ident) => {
                        let edition = ctx_edition(ident.span.ctx);
                        match ident.sym.as_str() {
                            "_" => res.push(T![_], edition),
                            i if i.starts_with('\'') => res.push(LIFETIME_IDENT, edition),
                            _ if ident.is_raw.yes() => res.push(IDENT, edition),
                            text => match SyntaxKind::from_keyword(text, edition) {
                                Some(kind) => res.push(kind, edition),
                                None => {
                                    let contextual_keyword =
                                        SyntaxKind::from_contextual_keyword(text, edition)
                                            .unwrap_or(SyntaxKind::IDENT);
                                    res.push_ident(contextual_keyword, edition);
                                }
                            },
                        }
                    }
                    tt::Leaf::Punct(punct) => {
                        let kind = SyntaxKind::from_char(punct.char)
                            .unwrap_or_else(|| panic!("{punct:#?} is not a valid punct"));
                        res.push(kind, ctx_edition(punct.span.ctx));
                        if punct.spacing == tt::Spacing::Joint {
                            res.was_joint();
                        }
                    }
                }
                current.bump();
            }
            Some(tt::TokenTree::Subtree(subtree)) => {
                if let Some(kind) = match subtree.delimiter.kind {
                    tt::DelimiterKind::Parenthesis => Some(T!['(']),
                    tt::DelimiterKind::Brace => Some(T!['{']),
                    tt::DelimiterKind::Bracket => Some(T!['[']),
                    tt::DelimiterKind::Invisible => None,
                } {
                    res.push(kind, ctx_edition(subtree.delimiter.open.ctx));
                } else if fragments && fragment_at.is_none() {
                    fragment_at = Some(res.len());
                }
                current.bump();
            }
            None => {
                let subtree = current.end();
                if let Some(kind) = match subtree.delimiter.kind {
                    tt::DelimiterKind::Parenthesis => Some(T![')']),
                    tt::DelimiterKind::Brace => Some(T!['}']),
                    tt::DelimiterKind::Bracket => Some(T![']']),
                    tt::DelimiterKind::Invisible => None,
                } {
                    res.push(kind, ctx_edition(subtree.delimiter.close.ctx));
                }
            }
        };
    }
    if let Some(at) = fragment_at
        && res.len() > at
    {
        res.mark_fragment_start(at);
    }

    res
}
