//! Token streams to and from the token trees of the proc-macro wire (`tt::TopSubtree::read_wire`
//! and `write_wire`, protocol version `WIRE_TREES`): a request's trees read as the streams a macro
//! is given, and its output written as one tree. Both walk with a stack of their own, so a tree
//! nested however deep costs no native stack.

use intern::Symbol;
use span::{Span, SpanAnchor, SyntaxContext};
use tt::{TopSubtree, TopSubtreeBuilder, TtElement, WirePart};

use crate::{
    Delimiter, TokenStream, TokenStreamIter,
    bridge::{DelimSpan, Group, Ident, IdentKind, LitKind, Literal, Punct, TokenTree},
};

/// The longest suffix a token tree's literal holds: its node keeps the length in 18 bits.
const LONGEST_SUFFIX: usize = (u32::MAX >> 14) as usize;

impl TokenStream<Span> {
    /// Reads a tree `write_wire` wrote at the start of `bytes` as the stream a macro is given:
    /// the top subtree's token trees when it is invisible, else the one group it is. Returns the
    /// stream and how many bytes it took. `part` turns each span part's raw ids into an anchor
    /// and a context, or refuses them; `join` makes a group's whole span of its delimiters'.
    pub fn read_wire(
        bytes: &[u8],
        part: &mut impl FnMut(WirePart) -> Result<(SpanAnchor, SyntaxContext), String>,
        join: impl Fn(Span, Span) -> Span,
    ) -> Result<(TokenStream<Span>, usize), String> {
        let (tree, used) = TopSubtree::read_wire(bytes, part).map_err(|error| error.to_string())?;
        let top = tree.top_subtree().delimiter;
        let stream = streams(tree.iter(), &join)?;
        let stream = match top.kind {
            tt::DelimiterKind::Invisible => stream,
            _ => TokenStream::new(vec![TokenTree::Group(group(top, stream, &join))]),
        };
        Ok((stream, used))
    }

    /// Appends the stream to `out` as `write_wire` writes a tree: the one group it is as the top
    /// subtree, else an invisible top at `call_site` holding it. A negative number is a `-` and
    /// its magnitude, as rustc's parser takes the literal a macro makes of one.
    pub fn write_wire(&self, call_site: Span, out: &mut Vec<u8>) -> Result<(), String> {
        let empty = TokenStream::default();
        let (top, contents) = match self.as_single_group() {
            Some(group) => (delimiter(group), group.stream.as_ref().unwrap_or(&empty)),
            None => (tt::Delimiter::invisible_spanned(call_site), self),
        };
        let mut builder = TopSubtreeBuilder::new(top);
        let mut open: Vec<(TokenStreamIter<'_, Span>, Span)> = Vec::new();
        let mut trees = contents.iter();
        loop {
            let Some(tree) = trees.next() else {
                let Some((outer, close)) = open.pop() else { break };
                builder.close(close);
                trees = outer;
                continue;
            };
            match tree {
                TokenTree::Group(group) => {
                    let inner = group.stream.as_ref().unwrap_or(&empty).iter();
                    builder.open(delimiter(group).kind, group.span.open);
                    open.push((std::mem::replace(&mut trees, inner), group.span.close));
                }
                TokenTree::Literal(literal) => push_literal(&mut builder, literal)?,
                TokenTree::Punct(punct) => builder.push(tt::Leaf::Punct(tt::Punct {
                    char: char::from(punct.ch),
                    spacing: if punct.joint { tt::Spacing::Joint } else { tt::Spacing::Alone },
                    span: punct.span,
                })),
                TokenTree::Ident(ident) => builder.push(tt::Leaf::Ident(tt::Ident {
                    sym: ident.sym,
                    span: ident.span,
                    is_raw: match ident.kind {
                        IdentKind::Raw => tt::IdentIsRaw::Yes,
                        _ => tt::IdentIsRaw::No,
                    },
                })),
            }
        }
        builder.build().write_wire(out);
        Ok(())
    }
}

fn push_literal(builder: &mut TopSubtreeBuilder, literal: &Literal<Span>) -> Result<(), String> {
    let suffix = literal.suffix.as_ref().map_or("", |it| it.as_str());
    if suffix.len() > LONGEST_SUFFIX {
        return Err(format!(
            "a literal with a suffix of {} bytes, past the {LONGEST_SUFFIX} a token tree holds",
            suffix.len()
        ));
    }
    let mut text = literal.symbol.as_str();
    if matches!(literal.kind, LitKind::Integer | LitKind::Float)
        && let Some(magnitude) = text.strip_prefix('-')
    {
        let minus = tt::Punct { char: '-', spacing: tt::Spacing::Alone, span: literal.span };
        builder.push(tt::Leaf::Punct(minus));
        text = magnitude;
    }
    let kind = match literal.kind {
        LitKind::Byte => tt::LitKind::Byte,
        LitKind::Char => tt::LitKind::Char,
        LitKind::Integer => tt::LitKind::Integer,
        LitKind::Float => tt::LitKind::Float,
        LitKind::Str => tt::LitKind::Str,
        LitKind::StrRaw(hashes) => tt::LitKind::StrRaw(hashes),
        LitKind::ByteStr => tt::LitKind::ByteStr,
        LitKind::ByteStrRaw(hashes) => tt::LitKind::ByteStrRaw(hashes),
        LitKind::CStr => tt::LitKind::CStr,
        LitKind::CStrRaw(hashes) => tt::LitKind::CStrRaw(hashes),
        LitKind::ErrWithGuar => tt::LitKind::Err(()),
    };
    builder.push(tt::Leaf::Literal(tt::Literal::new(text, literal.span, kind, suffix)));
    Ok(())
}

/// The streams of the token trees `top` walks, each subtree a group.
fn streams(
    top: tt::TtIter<'_>,
    join: &impl Fn(Span, Span) -> Span,
) -> Result<TokenStream<Span>, String> {
    let mut open: Vec<(Vec<TokenTree<Span>>, tt::TtIter<'_>, tt::Delimiter)> = Vec::new();
    let mut trees = Vec::new();
    let mut iter = top;
    loop {
        match iter.next() {
            Some(TtElement::Leaf(leaf)) => trees.push(leaf_tree(leaf)?),
            Some(TtElement::Subtree(subtree, inner)) => {
                let outer = std::mem::replace(&mut iter, inner);
                open.push((std::mem::take(&mut trees), outer, subtree.delimiter));
            }
            None => {
                let Some((outer, rest, delimiter)) = open.pop() else {
                    return Ok(TokenStream::new(trees));
                };
                let inner = TokenStream::new(std::mem::replace(&mut trees, outer));
                trees.push(TokenTree::Group(group(delimiter, inner, join)));
                iter = rest;
            }
        }
    }
}

fn group(
    delimiter: tt::Delimiter,
    stream: TokenStream<Span>,
    join: &impl Fn(Span, Span) -> Span,
) -> Group<Span> {
    Group {
        delimiter: match delimiter.kind {
            tt::DelimiterKind::Parenthesis => Delimiter::Parenthesis,
            tt::DelimiterKind::Brace => Delimiter::Brace,
            tt::DelimiterKind::Bracket => Delimiter::Bracket,
            tt::DelimiterKind::Invisible => Delimiter::None,
        },
        stream: if stream.is_empty() { None } else { Some(stream) },
        span: DelimSpan {
            open: delimiter.open,
            close: delimiter.close,
            entire: join(delimiter.open, delimiter.close),
        },
    }
}

fn delimiter(group: &Group<Span>) -> tt::Delimiter {
    tt::Delimiter {
        open: group.span.open,
        close: group.span.close,
        kind: match group.delimiter {
            Delimiter::Parenthesis => tt::DelimiterKind::Parenthesis,
            Delimiter::Brace => tt::DelimiterKind::Brace,
            Delimiter::Bracket => tt::DelimiterKind::Bracket,
            Delimiter::None => tt::DelimiterKind::Invisible,
        },
    }
}

fn leaf_tree(leaf: tt::Leaf) -> Result<TokenTree<Span>, String> {
    Ok(match leaf {
        tt::Leaf::Literal(literal) => {
            let (text, suffix) = literal.text_and_suffix();
            TokenTree::Literal(Literal {
                kind: match literal.kind {
                    tt::LitKind::Byte => LitKind::Byte,
                    tt::LitKind::Char => LitKind::Char,
                    tt::LitKind::Integer => LitKind::Integer,
                    tt::LitKind::Float => LitKind::Float,
                    tt::LitKind::Str => LitKind::Str,
                    tt::LitKind::StrRaw(hashes) => LitKind::StrRaw(hashes),
                    tt::LitKind::ByteStr => LitKind::ByteStr,
                    tt::LitKind::ByteStrRaw(hashes) => LitKind::ByteStrRaw(hashes),
                    tt::LitKind::CStr => LitKind::CStr,
                    tt::LitKind::CStrRaw(hashes) => LitKind::CStrRaw(hashes),
                    tt::LitKind::Err(()) => LitKind::ErrWithGuar,
                },
                symbol: Symbol::intern(text),
                suffix: (!suffix.is_empty()).then(|| Symbol::intern(suffix)),
                span: literal.span,
            })
        }
        tt::Leaf::Punct(punct) => {
            if !punct.char.is_ascii() {
                return Err(format!("a punct {:?}, which is not ASCII", punct.char));
            }
            TokenTree::Punct(Punct {
                ch: punct.char as u8,
                joint: punct.spacing == tt::Spacing::Joint,
                span: punct.span,
            })
        }
        tt::Leaf::Ident(ident) => TokenTree::Ident(Ident {
            sym: ident.sym,
            span: ident.span,
            kind: match ident.is_raw {
                tt::IdentIsRaw::Yes => IdentKind::Raw,
                tt::IdentIsRaw::No => IdentKind::Normal,
            },
        }),
    })
}

#[cfg(test)]
mod tests {
    use span::{EditionedFileId, FileId, ROOT_ERASED_FILE_AST_ID, TextRange, TextSize};

    use super::*;

    fn span(start: u32, ctx: u32) -> Span {
        Span {
            range: TextRange::at(TextSize::new(start), TextSize::new(1)),
            anchor: SpanAnchor {
                file_id: EditionedFileId::new(FileId::from_raw(1), span::Edition::CURRENT),
                ast_id: ROOT_ERASED_FILE_AST_ID,
            },
            // SAFETY: the server has no database; the context is carried and compared only.
            ctx: unsafe { SyntaxContext::from_u32(ctx) },
        }
    }

    fn parts(tree: &TopSubtree) -> impl FnMut(WirePart) -> Result<(SpanAnchor, SyntaxContext), String> {
        let anchor = tree.top_subtree().delimiter.open.anchor;
        move |part| {
            // SAFETY: as in `span`.
            Ok((anchor, unsafe { SyntaxContext::from_u32(part.ctx) }))
        }
    }

    fn tree(kind: tt::DelimiterKind) -> TopSubtree {
        let mut builder = TopSubtreeBuilder::new(tt::Delimiter {
            open: span(0, 0),
            close: span(40, 0),
            kind,
        });
        builder.push(tt::Leaf::Ident(tt::Ident {
            sym: Symbol::intern("fn"),
            span: span(1, 1),
            is_raw: tt::IdentIsRaw::Yes,
        }));
        for depth in 0..100 {
            builder.open(tt::DelimiterKind::Parenthesis, span(2 + depth, 2));
        }
        builder.push(tt::Leaf::Punct(tt::Punct {
            char: '-',
            spacing: tt::Spacing::Alone,
            span: span(3, 2),
        }));
        builder.push(tt::Leaf::Literal(tt::Literal::new("1", span(3, 2), tt::LitKind::Integer, "u8")));
        builder.push(tt::Leaf::Literal(tt::Literal::new("a", span(4, 0), tt::LitKind::StrRaw(2), "")));
        for depth in 0..100 {
            builder.close(span(30 + depth, 2));
        }
        builder.build()
    }

    #[test]
    fn a_tree_read_as_a_stream_writes_back_the_same_bytes() {
        for kind in [tt::DelimiterKind::Invisible, tt::DelimiterKind::Brace] {
            let tree = tree(kind);
            let mut bytes = Vec::new();
            tree.write_wire(&mut bytes);
            let (stream, used) =
                TokenStream::read_wire(&bytes, &mut parts(&tree), |open, _| open).unwrap();
            assert_eq!(used, bytes.len());
            let mut again = Vec::new();
            stream.write_wire(span(0, 0), &mut again).unwrap();
            // An invisible top is the call site's once the stream is written back.
            if kind != tt::DelimiterKind::Invisible {
                assert_eq!(again, bytes, "{kind:?}");
            }
            let (back, _) = TopSubtree::read_wire(&again, &mut parts(&tree)).unwrap();
            let tokens = |tree: &TopSubtree| tree.token_trees().iter_flat_tokens().collect::<Vec<_>>();
            assert_eq!(tokens(&back), tokens(&tree), "{kind:?}");
            assert_eq!(tokens(&back).len(), 104);
        }
    }

    #[test]
    fn a_negative_number_is_written_as_a_minus_and_its_magnitude() {
        let literal = Literal {
            kind: LitKind::Integer,
            symbol: Symbol::intern("-1"),
            suffix: Some(Symbol::intern("i32")),
            span: span(0, 0),
        };
        let stream = TokenStream::new(vec![TokenTree::Literal(literal)]);
        let mut bytes = Vec::new();
        stream.write_wire(span(0, 0), &mut bytes).unwrap();
        let sent = tree(tt::DelimiterKind::Invisible);
        let (read, _) = TopSubtree::read_wire(&bytes, &mut parts(&sent)).unwrap();
        let tokens: Vec<_> = read.token_trees().iter_flat_tokens().collect();
        assert!(matches!(&tokens[0], tt::TokenTree::Leaf(tt::Leaf::Punct(p)) if p.char == '-'));
        assert!(
            matches!(&tokens[1], tt::TokenTree::Leaf(tt::Leaf::Literal(l)) if l.text_and_suffix() == ("1", "i32"))
        );
    }
}
