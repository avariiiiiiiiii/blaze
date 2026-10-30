use native_proc_macro::{Ident, Span as MacroSpan, TokenTree as MacroTree};
use span::{EditionedFileId, ErasedFileAstId, SpanAnchor, SyntaxContext, TextRange};
use tt::{IdentIsRaw, LitKind, Spacing};

use super::{wire::Tree, *};

const GUEST: &str = include_str!("../../../blaze-macro-wasm/guest/proc_macro.rs");
const WASM_WIRE: &str = include_str!("../../../blaze-macro-wasm/src/wire.rs");
const WASM_LEX: &str = include_str!("../../../blaze-macro-wasm/src/lex.rs");

fn span(start: u32, end: u32) -> Span {
    let file_id = EditionedFileId::new(span::FileId::from_raw(7), span::Edition::CURRENT);
    Span {
        range: TextRange::new(start.into(), end.into()),
        anchor: SpanAnchor { file_id, ast_id: span::ROOT_ERASED_FILE_AST_ID },
        ctx: SyntaxContext::root(span::Edition::CURRENT),
    }
}

fn tokens(text: &str, at: Span) -> TopSubtree {
    reference::top_subtree(lex::stream(text, at).expect("the test's text lexes"), at)
}

/// The wasm host's reading of a module's output (`crates/blaze-macro-wasm/src/wire.rs`,
/// `Reader` and `top_subtree`): trees, then rust-analyzer's flat tree of them, which
/// [`wire::Reader`] makes in one pass.
mod reference {
    use intern::Symbol;
    use span::Span;
    use tt::{DelimiterKind, IdentIsRaw, Leaf, LitKind, Spacing, Subtree, TokenTree, TopSubtree};

    use super::wire::{Spans, Tree};

    pub(super) fn read(bytes: &[u8], spans: &Spans, call_site: Span) -> Result<TopSubtree, String> {
        let mut reader = Reader { bytes, at: 0, spans };
        Ok(top_subtree(reader.stream()?, call_site))
    }

    struct Reader<'a> {
        bytes: &'a [u8],
        at: usize,
        spans: &'a Spans,
    }

    impl<'a> Reader<'a> {
        fn take(&mut self, len: usize) -> Result<&'a [u8], String> {
            let end = self.at.checked_add(len).filter(|it| *it <= self.bytes.len());
            let end = end.ok_or("the module wrote a stream that ends early")?;
            let it = &self.bytes[self.at..end];
            self.at = end;
            Ok(it)
        }

        fn u8(&mut self) -> Result<u8, String> {
            Ok(self.take(1)?[0])
        }

        fn u32(&mut self) -> Result<u32, String> {
            Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
        }

        fn span(&mut self) -> Result<Span, String> {
            let number = self.u32()?;
            self.spans.get(number).ok_or_else(|| format!("the module named span {number}, unknown"))
        }

        fn text(&mut self) -> Result<&'a str, String> {
            let len = self.u32()? as usize;
            std::str::from_utf8(self.take(len)?).map_err(|it| it.to_string())
        }

        fn stream(&mut self) -> Result<Vec<Tree>, String> {
            let count = self.u32()? as usize;
            let mut trees = Vec::with_capacity(count.min(1 << 16));
            for _ in 0..count {
                trees.push(self.tree()?);
            }
            Ok(trees)
        }

        fn tree(&mut self) -> Result<Tree, String> {
            Ok(match self.u8()? {
                0 => {
                    let kind = match self.u8()? {
                        0 => DelimiterKind::Parenthesis,
                        1 => DelimiterKind::Brace,
                        2 => DelimiterKind::Bracket,
                        _ => DelimiterKind::Invisible,
                    };
                    let (open, close, entire) = (self.span()?, self.span()?, self.span()?);
                    Tree::Group { kind, open, close, entire, trees: self.stream()? }
                }
                1 => {
                    let span = self.span()?;
                    let is_raw = if self.u8()? == 1 { IdentIsRaw::Yes } else { IdentIsRaw::No };
                    Tree::Leaf(Leaf::Ident(tt::Ident {
                        sym: Symbol::intern(self.text()?),
                        span,
                        is_raw,
                    }))
                }
                2 => {
                    let span = self.span()?;
                    let char = char::from_u32(self.u32()?).ok_or("the module wrote no character")?;
                    let spacing = if self.u8()? != 0 { Spacing::Joint } else { Spacing::Alone };
                    Tree::Leaf(Leaf::Punct(tt::Punct { char, spacing, span }))
                }
                3 => {
                    let span = self.span()?;
                    let (kind, hashes) = (self.u8()?, self.u8()?);
                    let text = self.text()?;
                    let suffix = match u32::from_le_bytes(
                        self.bytes
                            .get(self.at..self.at + 4)
                            .ok_or("a literal ends early")?
                            .try_into()
                            .unwrap(),
                    ) {
                        u32::MAX => {
                            self.at += 4;
                            ""
                        }
                        _ => self.text()?,
                    };
                    let kind = match kind {
                        0 => LitKind::Byte,
                        1 => LitKind::Char,
                        2 => LitKind::Integer,
                        3 => LitKind::Float,
                        4 => LitKind::Str,
                        5 => LitKind::StrRaw(hashes),
                        6 => LitKind::ByteStr,
                        7 => LitKind::ByteStrRaw(hashes),
                        8 => LitKind::CStr,
                        9 => LitKind::CStrRaw(hashes),
                        _ => LitKind::Err(()),
                    };
                    Tree::Leaf(Leaf::Literal(tt::Literal::new(text, span, kind, suffix)))
                }
                other => return Err(format!("the module wrote a tree of kind {other}")),
            })
        }
    }

    pub(super) fn top_subtree(trees: Vec<Tree>, call_site: Span) -> TopSubtree {
        let mut flat = Vec::new();
        match <[Tree; 1]>::try_from(trees) {
            Ok([group @ Tree::Group { .. }]) => flatten(group, &mut flat),
            Ok([leaf]) => flatten(top_of(vec![leaf], call_site), &mut flat),
            Err(trees) => flatten(top_of(trees, call_site), &mut flat),
        }
        TopSubtree::from_serialized(flat)
    }

    fn top_of(trees: Vec<Tree>, call_site: Span) -> Tree {
        let (open, close, entire) = (call_site, call_site, call_site);
        Tree::Group { kind: DelimiterKind::Invisible, open, close, entire, trees }
    }

    fn flatten(tree: Tree, flat: &mut Vec<TokenTree>) {
        match tree {
            Tree::Leaf(Leaf::Literal(literal))
                if matches!(literal.kind, LitKind::Integer | LitKind::Float)
                    && literal.text().starts_with('-') =>
            {
                let (text, suffix) = literal.text_and_suffix();
                let span = literal.span;
                let minus = tt::Punct { char: '-', spacing: Spacing::Alone, span };
                let magnitude = tt::Literal::new(&text[1..], span, literal.kind, suffix);
                flat.push(TokenTree::Leaf(Leaf::Punct(minus)));
                flat.push(TokenTree::Leaf(Leaf::Literal(magnitude)));
            }
            Tree::Leaf(leaf) => flat.push(TokenTree::Leaf(leaf)),
            Tree::Group { kind, open, close, entire: _, trees } => {
                let at = flat.len();
                flat.push(TokenTree::Subtree(Subtree {
                    delimiter: tt::Delimiter { open, close, kind },
                    len: 0,
                }));
                for tree in trees {
                    flatten(tree, flat);
                }
                let len = (flat.len() - at - 1) as u32;
                if let TokenTree::Subtree(subtree) = &mut flat[at] {
                    subtree.len = len;
                }
            }
        }
    }
}

#[test]
fn the_one_pass_reader_reads_as_the_wasm_host_does() {
    let at = span(2, 40);
    let group = |kind, trees| Tree::Group { kind, open: span(1, 2), close: span(8, 9), entire: span(1, 9), trees };
    let lexed = |text: &str| lex::stream(text, at).unwrap();
    let negative = |text: &str| Tree::Leaf(Leaf::Literal(lex::literal_from_str(text, at).unwrap()));
    let streams: Vec<Vec<Tree>> = vec![
        vec![],
        lexed("x"),
        vec![negative("-1i32")],
        vec![negative("-2.5")],
        lexed("a::b(1u8, \"s\", r#\"raw\"#) { c[0] } 'a"),
        vec![group(tt::DelimiterKind::Brace, lexed("struct S { x: i32 }"))],
        vec![group(tt::DelimiterKind::Parenthesis, vec![negative("-7")]), Tree::Leaf(Leaf::Punct(tt::Punct { char: ';', spacing: Spacing::Alone, span: at }))],
        vec![group(tt::DelimiterKind::Invisible, lexed("r#type + 1"))],
    ];
    for trees in streams {
        let mut spans = Spans::default();
        let mut writer = Writer { out: Vec::new(), spans: &mut spans };
        writer.trees(&trees);
        let bytes = writer.out;
        let ours = wire::Reader::new(&bytes, &spans).top(at).unwrap();
        let theirs = reference::read(&bytes, &spans, at).unwrap();
        assert_eq!(difference(&ours, &theirs), None);
        assert!(ours == theirs);
    }
    let spans = Spans::default();
    assert!(wire::Reader::new(&[1, 0, 0, 0], &spans).top(at).is_err());
    assert!(reference::read(&[1, 0, 0, 0], &spans, at).is_err());
}

/// The text from the first `from` to the next `to` after it.
fn between<'a>(text: &'a str, from: &str, to: &str) -> &'a str {
    let start = text.find(from).unwrap_or_else(|| panic!("no {from:?}"));
    let end = start + text[start..].find(to).unwrap_or_else(|| panic!("no {to:?} after {from:?}"));
    &text[start..end]
}

#[test]
fn the_proc_macro_is_the_guests_but_for_its_imports_and_exports() {
    let shim = include_str!("proc_macro/lib.rs");
    assert_eq!(
        between(shim, "\nfn taken(", "\n/// How blaze runs a macro natively"),
        between(GUEST, "\nfn taken(", "\n/// What blaze's list of a crate's macros calls"),
    );
    let uses = |text: &'static str| between(text, "\nuse std::{", "};");
    assert_eq!(uses(shim), uses(GUEST));
}

#[test]
fn the_wire_and_the_lexer_are_the_wasm_hosts() {
    let from = |text: &'static str, start: &str| &text[text.find(start).unwrap()..];
    let numbering = |text: &'static str| between(text, "\nuse intern::Symbol;", "\n/// Reads what the module wrote");
    assert_eq!(numbering(include_str!("wire.rs")), numbering(WASM_WIRE));
    let lex = from(include_str!("lex.rs"), "\nuse intern::Symbol;");
    let wasm_lex = from(WASM_LEX, "\nuse intern::Symbol;")
        .replacen("use crate::wire::Tree;", "use super::wire::Tree;", 1);
    assert_eq!(lex, wasm_lex);
}

#[test]
fn equal_trees_have_no_difference() {
    let text = "#[derive(Debug)] struct S<T> { a: [u8; 3], b: (T, \"s\", 2.5e3f64) }";
    let tree = tokens(text, span(3, 9));
    assert_eq!(difference(&tree, &tokens(text, span(3, 9))), None);
}

/// `tree` with its token tree `at` changed by `change`.
fn changed(tree: &TopSubtree, at: usize, change: impl FnOnce(&mut TokenTree)) -> TopSubtree {
    let mut flat: Vec<TokenTree> = tree.as_token_trees().iter_flat_tokens().collect();
    change(&mut flat[at]);
    TopSubtree::from_serialized(flat)
}

fn leaf_span(tree: &mut TokenTree) -> &mut Span {
    match tree {
        TokenTree::Leaf(Leaf::Ident(it)) => &mut it.span,
        TokenTree::Leaf(Leaf::Punct(it)) => &mut it.span,
        TokenTree::Leaf(Leaf::Literal(it)) => &mut it.span,
        TokenTree::Subtree(it) => &mut it.delimiter.open,
    }
}

#[test]
fn a_difference_names_what_differs() {
    // 0 the invisible top, 1 `a`, 2 `:`, 3 `:`, 4 `b`, 5 `(`, 6 `1u8`.
    let tree = tokens("a::b(1u8)", span(0, 9));
    let said = |other: &TopSubtree| difference(other, &tree).expect("a difference");

    let renamed = changed(&tree, 1, |it| {
        if let TokenTree::Leaf(Leaf::Ident(ident)) = it {
            ident.sym = intern::Symbol::intern("c");
        }
    });
    assert_eq!(said(&renamed), "token tree 1, `a`: its text is \"c\" natively and \"a\" by the macro");

    let raw = changed(&tree, 1, |it| {
        if let TokenTree::Leaf(Leaf::Ident(ident)) = it {
            ident.is_raw = IdentIsRaw::Yes;
        }
    });
    assert!(said(&raw).contains("its rawness is Yes natively"), "{}", said(&raw));

    let spaced = changed(&tree, 2, |it| {
        if let TokenTree::Leaf(Leaf::Punct(punct)) = it {
            punct.spacing = Spacing::Alone;
        }
    });
    assert_eq!(said(&spaced), "token tree 2, `:`: its spacing is Alone natively and Joint by the macro");

    let moved = changed(&tree, 4, |it| leaf_span(it).range = TextRange::new(1.into(), 2.into()));
    assert!(said(&moved).starts_with("token tree 4, `b`: its range is 1..2 natively"), "{}", said(&moved));

    let anchored = changed(&tree, 6, |it| leaf_span(it).anchor.ast_id = ErasedFileAstId::from_raw(1));
    assert!(said(&anchored).starts_with("token tree 6, the literal `1u8`: its anchor is"), "{}", said(&anchored));

    let hygienic = changed(&tree, 5, |it| {
        leaf_span(it).ctx = SyntaxContext::root(span::Edition::Edition2015);
    });
    assert!(
        said(&hygienic).starts_with("token tree 5, the group `(`: its opening delimiter's syntax context is"),
        "{}",
        said(&hygienic)
    );

    let suffixed = changed(&tree, 6, |it| {
        if let TokenTree::Leaf(Leaf::Literal(literal)) = it {
            *literal = tt::Literal::new("1", literal.span, LitKind::Integer, "u16");
        }
    });
    assert!(said(&suffixed).contains("its suffix is \"u16\" natively and \"u8\" by the macro"));

    let shorter = tokens("a::b", span(0, 9));
    assert!(difference(&shorter, &tree).is_some());
}

/// The text of each token of `tree` in order, a group as its opening delimiter.
fn words(tree: &TopSubtree) -> Vec<String> {
    tree.as_token_trees()
        .iter_flat_tokens()
        .filter_map(|it| match it {
            TokenTree::Subtree(subtree) => match subtree.delimiter.kind {
                tt::DelimiterKind::Parenthesis => Some("(".to_owned()),
                tt::DelimiterKind::Brace => Some("{".to_owned()),
                tt::DelimiterKind::Bracket => Some("[".to_owned()),
                tt::DelimiterKind::Invisible => None,
            },
            TokenTree::Leaf(Leaf::Ident(it)) => {
                Some(format!("{}{}", it.is_raw.as_str(), it.sym.as_str()))
            }
            TokenTree::Leaf(Leaf::Punct(it)) => Some(it.char.to_string()),
            TokenTree::Leaf(Leaf::Literal(it)) if it.kind == LitKind::Str => {
                Some(format!("\"{}\"", it.text()))
            }
            TokenTree::Leaf(Leaf::Literal(it)) => Some(format!("{}{}", it.text(), it.suffix())),
        })
        .collect()
}

#[track_caller]
fn assert_has(tree: &TopSubtree, window: &str) {
    let words = words(tree);
    let window: Vec<&str> = window.split(' ').collect();
    assert!(
        words.windows(window.len()).any(|it| it.iter().zip(&window).all(|(a, b)| a == b)),
        "no `{}` in `{}`",
        window.join(" "),
        words.join(" "),
    );
}

/// Every span of `tree` is of the test's one file and of its one context.
#[track_caller]
fn assert_spans_known(tree: &TopSubtree) {
    for it in tree.as_token_trees().iter_flat_tokens() {
        let spans = match &it {
            TokenTree::Subtree(it) => vec![it.delimiter.open, it.delimiter.close],
            TokenTree::Leaf(it) => vec![*it.span()],
        };
        for it in spans {
            assert_eq!((it.anchor, it.ctx), (span(0, 0).anchor, span(0, 0).ctx), "{it:?}");
        }
    }
}

fn echo(input: TokenStream) -> TokenStream {
    input
}

fn parsed(_: TokenStream) -> TokenStream {
    "fn made() -> u8 { -1i8 as u8 }".parse().expect("the text lexes")
}

fn named(_: TokenStream) -> TokenStream {
    [Ident::new("at_call", MacroSpan::call_site()), Ident::new("at_mixed", MacroSpan::mixed_site())]
        .into_iter()
        .map(MacroTree::from)
        .collect()
}

fn attributed(attr: TokenStream, item: TokenStream) -> TokenStream {
    let mut out = attr;
    out.extend(item);
    out
}

fn panics(_: TokenStream) -> TokenStream {
    panic!("no {}", "way")
}

/// Expands another macro on this thread while it runs, as a host's answer can.
fn nested(input: TokenStream) -> TokenStream {
    let at = span(50, 55);
    let inner = tokens("y + 1", at);
    let out = expand(Expander::One(echo), None, &inner, None, at, at, at).expect("the inner expansion");
    assert_eq!(difference(&out, &inner), None);
    input
}

/// `expander` expanding `input` (and `attr`, for an attribute), its spans within the text of
/// each, with no database to ask; the call site is the input's span.
fn run(expander: Expander, attr: Option<&str>, input: &str) -> (TopSubtree, Result<TopSubtree, ProcMacroExpansionError>) {
    let at = span(0, input.len() as u32);
    let input = tokens(input, at);
    let attr = attr.map(|it| tokens(it, span(1000, 1000 + it.len() as u32)));
    let out = expand(expander, None, &input, attr.as_ref(), span(5, 5), at, span(7, 7));
    (input, out)
}

#[test]
fn a_macro_gets_its_input_as_it_was_given() {
    for text in ["a::b(1u8, -2, \"s\") { c[0] } 'a", "x", "struct S;", "r#type - 2.5"] {
        let (input, out) = run(Expander::One(echo), None, text);
        assert_eq!(difference(&out.unwrap(), &input), None, "{text}");
    }
}

#[test]
fn a_parsed_stream_is_at_the_call_site() {
    let (_, out) = run(Expander::One(parsed), None, "anything here");
    let out = out.unwrap();
    assert_has(&out, "fn made ( ) - > u8 { - 1i8 as u8 }");
    assert_spans_known(&out);
}

#[test]
fn the_sites_are_the_calls() {
    let (_, out) = run(Expander::One(named), None, "a b");
    let out = out.unwrap();
    assert_eq!(words(&out), ["at_call", "at_mixed"]);
    let spans: Vec<Span> = out
        .as_token_trees()
        .iter_flat_tokens()
        .filter_map(|it| match it {
            TokenTree::Leaf(Leaf::Ident(it)) => Some(it.span),
            _ => None,
        })
        .collect();
    assert_eq!(spans, [span(0, 3), span(7, 7)]);
}

#[test]
fn an_attribute_macro_gets_its_attribute_then_its_item() {
    let (input, out) = run(Expander::Two(attributed), None, "fn f() {}");
    assert_eq!(difference(&out.unwrap(), &input), None, "no attribute is an empty one");
    let (input, out) = run(Expander::Two(attributed), Some("x y"), "fn f() {}");
    let mut expected = vec!["x".to_owned(), "y".to_owned()];
    expected.extend(words(&input));
    assert_eq!(words(&out.unwrap()), expected);
}

#[test]
fn a_panic_is_its_message() {
    let (_, out) = run(Expander::One(panics), None, "a");
    assert!(matches!(&out, Err(ProcMacroExpansionError::Panic(it)) if it == "no way"), "{out:?}");
    let (input, out) = run(Expander::One(echo), None, "after a panic");
    assert_eq!(difference(&out.unwrap(), &input), None);
}

#[test]
fn an_expansion_within_an_expansion_has_its_own_host_and_buffers() {
    let (input, out) = run(Expander::One(nested), None, "a + b");
    assert_eq!(difference(&out.unwrap(), &input), None);
}
