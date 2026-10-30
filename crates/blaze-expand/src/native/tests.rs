use blaze_ids::rng::SplitMix64;
use native_proc_macro::{
    Ident, Literal as MacroLiteral, Span as MacroSpan, TokenTree as MacroTree, __native::with_host,
};
use span::{EditionedFileId, ErasedFileAstId, SpanAnchor, SyntaxContext, TextRange};
use tt::{IdentIsRaw, LitKind, Spacing};

use super::*;
use crate::bridge::{
    lex,
    wire::{self, Parts},
};

/// The wasm guest's own text: its imports (`sys`), allocator and exports, which
/// `blaze_macro_wasm::GUEST` follows with `proc_macro/api.rs`.
const GUEST: &str = include_str!("../../../blaze-macro-wasm/guest/proc_macro.rs");

fn span(start: u32, end: u32) -> Span {
    let file_id = EditionedFileId::new(span::FileId::from_raw(7), span::Edition::CURRENT);
    Span {
        range: TextRange::new(start.into(), end.into()),
        anchor: SpanAnchor { file_id, ast_id: span::ROOT_ERASED_FILE_AST_ID },
        ctx: SyntaxContext::root(span::Edition::CURRENT),
    }
}

fn tokens(text: &str, at: Span) -> TopSubtree {
    wire::top_subtree(lex::stream(text, at).expect("the test's text lexes"), at)
}

/// What the wasm host and its module do with an expansion, in process: the input and the
/// attribute as the columns the host gives a module (`wire::relative`), read by the guest's
/// reader, the macro's output written by the guest's writer, with the host's answers to the
/// guest's imports. The parts of the expansion and the bytes written, which the host reads.
fn written(
    expander: Expander,
    input: &TopSubtree,
    attr: Option<&TopSubtree>,
    sites: [Span; 3],
) -> (Parts, Vec<u8>) {
    let mut parts = Parts::default();
    let words = sites.map(|it| parts.pack(it));
    let mut input_bytes = Vec::new();
    wire::relative(input, &mut parts, &mut input_bytes);
    let attr_bytes = attr.map(|it| {
        let mut bytes = Vec::new();
        wire::relative(it, &mut parts, &mut bytes);
        bytes
    });
    let mut out = Vec::new();
    let mut ask = host::Ask { db: None };
    let mut host = native_proc_macro::__native::Host { parts: &mut parts, sites: words, ask: &mut ask };
    with_host(&mut host, || {
        let read = |bytes: &[u8]| match bytes.len() {
            0 => TokenStream::new(),
            _ => native_proc_macro::wire::Reader::new(bytes).input(),
        };
        let output = match expander {
            Expander::One(f) => f(read(&input_bytes)),
            Expander::Two(f) => {
                let attr = attr_bytes.as_deref().map_or_else(TokenStream::new, read);
                f(attr, read(&input_bytes))
            }
        };
        native_proc_macro::wire::write(&output, &mut out);
    });
    (parts, out)
}

/// The tree the wasm host makes of an expansion ([`written`]).
fn through_columns(
    expander: Expander,
    input: &TopSubtree,
    attr: Option<&TopSubtree>,
    sites: [Span; 3],
) -> Result<TopSubtree, String> {
    let (parts, out) = written(expander, input, attr, sites);
    wire::read(&out, &parts)
}

/// Expands `input` both natively and as the wasm host does, which are to be the same tree.
#[track_caller]
fn both(expander: Expander, input: &TopSubtree, attr: Option<&TopSubtree>, sites: [Span; 3]) -> TopSubtree {
    let [def, call, mixed] = sites;
    let native = expand(expander, None, input, attr, def, call, mixed).expect("the native expansion");
    let wasm = through_columns(expander, input, attr, sites).expect("the host reads what the guest wrote");
    assert_eq!(difference(&native, &wasm), None);
    assert!(native == wasm);
    native
}

/// More names than a symbol is looked for among the latest of, each used again.
const NAMES: [&str; 24] = [
    "a", "b", "c", "d", "e", "f", "g", "h", "i", "j", "k", "l", "m", "n", "o", "p", "q", "r",
    "s", "t", "u", "v", "w", "x",
];

/// A span of one of `parts` anchors (file 7, an ast id each) and one of two contexts, now and
/// then longer than a packed span holds.
fn random_span(rng: &mut SplitMix64, parts: usize) -> Span {
    let file_id = EditionedFileId::new(span::FileId::from_raw(7), span::Edition::CURRENT);
    let edition = if rng.index(4) == 0 { span::Edition::Edition2015 } else { span::Edition::CURRENT };
    let start = rng.index(1 << 20) as u32;
    let len: u32 = match rng.index(16) {
        0 => 0x1_0000 + rng.index(1 << 16) as u32,
        _ => rng.index(64) as u32,
    };
    Span {
        range: TextRange::at(start.into(), len.into()),
        anchor: SpanAnchor { file_id, ast_id: ErasedFileAstId::from_raw(rng.index(parts) as u32) },
        ctx: SyntaxContext::root(edition),
    }
}

fn random_leaf(rng: &mut SplitMix64, span: Span) -> Leaf {
    const LITERALS: [(LitKind, &str); 11] = [
        (LitKind::Integer, "17"),
        (LitKind::Float, "2.5"),
        (LitKind::Str, "s"),
        (LitKind::StrRaw(2), "raw"),
        (LitKind::Byte, "b"),
        (LitKind::Char, "c"),
        (LitKind::ByteStr, "bs"),
        (LitKind::ByteStrRaw(1), "rb"),
        (LitKind::CStr, "cs"),
        (LitKind::CStrRaw(3), "rc"),
        (LitKind::Err(()), "err"),
    ];
    const SUFFIXES: [&str; 4] = ["", "u8", "f32", "a_suffix"];
    match rng.index(3) {
        0 => {
            let char = ['+', '-', '#', ':', '<', '&'][rng.index(6)];
            let spacing = if rng.index(2) == 0 { Spacing::Alone } else { Spacing::Joint };
            Leaf::Punct(tt::Punct { char, spacing, span })
        }
        1 => {
            let is_raw = if rng.index(8) == 0 { IdentIsRaw::Yes } else { IdentIsRaw::No };
            Leaf::Ident(tt::Ident { sym: intern::Symbol::intern(NAMES[rng.index(NAMES.len())]), span, is_raw })
        }
        _ => {
            let (kind, text) = LITERALS[rng.index(LITERALS.len())];
            Leaf::Literal(tt::Literal::new(text, span, kind, SUFFIXES[rng.index(SUFFIXES.len())]))
        }
    }
}

/// Token trees, at least two in every group so that none is a passed-on fragment and the top
/// is never one group: a tree an identity macro gives back as it is.
fn random_trees(rng: &mut SplitMix64, builder: &mut tt::TopSubtreeBuilder, parts: usize, budget: &mut usize, depth: u32) {
    let count = 2 + rng.index(6);
    for at in 0..count {
        if at >= 2 && *budget == 0 {
            break;
        }
        *budget = budget.saturating_sub(1);
        if depth < 6 && rng.index(5) == 0 {
            let kind = [
                tt::DelimiterKind::Parenthesis,
                tt::DelimiterKind::Brace,
                tt::DelimiterKind::Bracket,
                tt::DelimiterKind::Invisible,
            ][rng.index(4)];
            builder.open(kind, random_span(rng, parts));
            random_trees(rng, builder, parts, budget, depth + 1);
            builder.close(random_span(rng, parts));
        } else {
            let span = random_span(rng, parts);
            builder.push(random_leaf(rng, span));
        }
    }
}

fn random_tree(seed: u64, parts: usize, budget: usize, call_site: Span) -> TopSubtree {
    let mut rng = SplitMix64::new(seed);
    let mut builder = tt::TopSubtreeBuilder::new(tt::Delimiter::invisible_spanned(call_site));
    random_trees(&mut rng, &mut builder, parts, &mut { budget }, 0);
    builder.build()
}

/// What the test's macros are given and expand to read the same natively and as the wasm host
/// reads them: blaze's lexed text, of every kind of token.
const CORPUS: [&str; 10] = [
    "a::b(1u8, -2, \"s\") { c[0] } 'a",
    "x",
    "struct S;",
    "r#type - 2.5",
    "{ fn f() {} }",
    "#[derive(Debug)] struct S<T> { a: [u8; 3], b: (T, \"s\", b'x', r#\"raw\"#, 2.5e3f64) }",
    "c\"c\" cr#\"cr\"# br\"br\" b\"b\" 'c' 1_000i128 0x1F 1e-3",
    "a && b || !c ..= d ::< e >> f",
    "(())",
    "[a] (b) {c}",
];

#[test]
fn an_identity_macro_gives_back_every_tree_as_the_wasm_host_does() {
    for text in CORPUS {
        let at = span(0, text.len() as u32);
        let input = tokens(text, at);
        let out = both(Expander::One(echo), &input, None, [span(5, 5), at, span(7, 7)]);
        assert_eq!(difference(&out, &input), None, "{text}");
    }
    for seed in 0..64 {
        let at = span(3, 9);
        let input = random_tree(seed, 1 + (seed as usize % 40), 200, at);
        let out = both(Expander::One(echo), &input, None, [span(5, 5), at, span(7, 7)]);
        assert_eq!(difference(&out, &input), None, "seed {seed}");
        let attr = random_tree(seed + 1000, 3, 20, span(1000, 1010));
        both(Expander::Two(attributed), &input, Some(&attr), [span(5, 5), at, span(7, 7)]);
    }
}

#[test]
fn more_parts_than_a_packed_span_names_cross_as_wide_spans() {
    let at = span(0, 1);
    let mut builder = tt::TopSubtreeBuilder::new(tt::Delimiter::invisible_spanned(at));
    let file_id = EditionedFileId::new(span::FileId::from_raw(7), span::Edition::CURRENT);
    for ast in 0..70_000u32 {
        let anchor = SpanAnchor { file_id, ast_id: ErasedFileAstId::from_raw(ast) };
        let len: u32 = if ast % 7 == 0 { 0x2_0000 } else { 3 };
        let span = Span { range: TextRange::at(ast.into(), len.into()), anchor, ctx: at.ctx };
        let sym = intern::Symbol::intern(NAMES[ast as usize % NAMES.len()]);
        builder.push(Leaf::Ident(tt::Ident { sym, span, is_raw: IdentIsRaw::No }));
    }
    let input = builder.build();
    let out = both(Expander::One(echo), &input, None, [at; 3]);
    assert_eq!(difference(&out, &input), None);
}

fn negatives(input: TokenStream) -> TokenStream {
    let mut out = input;
    out.extend([
        MacroTree::from(MacroLiteral::i64_suffixed(-5)),
        MacroTree::from(MacroLiteral::f64_unsuffixed(-2.5)),
        MacroTree::from(MacroLiteral::i8_unsuffixed(-1)),
    ]);
    out
}

#[test]
fn what_the_guest_writes_the_host_reads_as_blazes_builder_makes_it() {
    for seed in 0..32 {
        let at = span(3, 9);
        let input = random_tree(seed, 1 + (seed as usize % 20), 300, at);
        let out = both(Expander::One(negatives), &input, None, [span(5, 5), at, span(7, 7)]);
        assert_has(&out, "- 5i64 - 2.5 - 1");
    }
}

/// Each tree of the input as the text the macro shows of it, which shows each literal kind its
/// own way, lexed again by the host; then a literal of each kind the macro can make.
fn reshown(input: TokenStream) -> TokenStream {
    let mut out: TokenStream = input
        .into_iter()
        .map(|tree| tree.to_string().parse::<TokenStream>().expect("what a tree shows lexes"))
        .collect();
    out.extend([
        MacroTree::from(MacroLiteral::string("s")),
        MacroTree::from(MacroLiteral::byte_string(b"b")),
        MacroTree::from(MacroLiteral::character('c')),
        MacroTree::from(MacroLiteral::byte_character(b'a')),
        MacroTree::from(MacroLiteral::u8_suffixed(1)),
        MacroTree::from(MacroLiteral::f64_unsuffixed(1.5)),
        MacroTree::from(MacroLiteral::c_string(c"c")),
    ]);
    out
}

/// Every kind but `Err`, which nothing lexes or makes, crosses both ways: a numbering of the
/// macro's (`api.rs`, `LitKind::code`) other than tt's would show a literal given as another
/// kind, or give back one made as another.
#[test]
fn every_literal_kind_is_numbered_as_tt_numbers_it() {
    let text = "b'a' 'c' 1 1.5 \"s\" r#\"r\"# b\"b\" br##\"b\"## c\"c\" cr#\"c\"#";
    let at = span(0, text.len() as u32);
    let input = tokens(text, at);
    let out = both(Expander::One(reshown), &input, None, [span(5, 5), at, span(7, 7)]);
    let literals = |tree: &TopSubtree| -> Vec<(LitKind, String)> {
        let flat = tree.as_token_trees().iter_flat_tokens();
        let text = |it: &tt::Literal| format!("{}{}", it.text(), it.suffix());
        flat.filter_map(|token| match token {
            TokenTree::Leaf(Leaf::Literal(it)) => Some((it.kind, text(&it))),
            _ => None,
        })
        .collect()
    };
    let mut want = literals(&input);
    assert_eq!(want.len(), 10, "the text has a literal of each kind that lexes");
    want.extend(
        [
            (LitKind::Str, "s"),
            (LitKind::ByteStr, "b"),
            (LitKind::Char, "c"),
            (LitKind::Byte, "a"),
            (LitKind::Integer, "1u8"),
            (LitKind::Float, "1.5"),
            (LitKind::CStr, "c"),
        ]
        .map(|(kind, text)| (kind, text.to_owned())),
    );
    assert_eq!(literals(&out), want);
}

#[test]
fn what_the_guest_writes_is_read_whole_or_refused() {
    let at = span(0, 20);
    let input = tokens("a (b [c]) \"d\" 1u8", at);
    let (parts, out) = written(Expander::One(echo), &input, None, [at; 3]);
    assert!(wire::read(&out, &parts).is_ok());
    for len in 0..out.len() {
        assert!(wire::read(&out[..len], &parts).is_err(), "{len} of {} bytes", out.len());
    }
    let mut longer = out.clone();
    longer.extend([0; 4]);
    assert!(wire::read(&longer, &parts).unwrap_err().contains("bytes after the token tree"));
    let row = wire::sections(&out).unwrap().parts;
    let mut past = out.clone();
    past[row..row + 4].copy_from_slice(&(parts.len() as u32).to_le_bytes());
    assert!(wire::read(&past, &parts).unwrap_err().contains("span parts"));
    let mut raw = out.clone();
    raw[row + 8..row + 12].copy_from_slice(&1u32.to_le_bytes());
    assert!(wire::read(&raw, &parts).unwrap_err().contains("span parts"));
}

#[test]
fn a_warm_expansion_allocates_only_the_macros_streams_and_its_output() {
    let at = span(0, 10);
    let input = tokens("a + (b, 1)", at);
    let run = || expand(Expander::One(echo), None, &input, None, at, at, at).unwrap();
    // Warms this thread's span parts and the builder's scratch.
    let out = run();
    let rebuild = || TopSubtree::from_token_trees(out.top_subtree().delimiter, out.token_trees());
    let (_, output) = blaze_alloc_count::measure(rebuild);
    // The macro's streams: two (the input's and its group's), each an `Rc` of a `Vec`, and
    // the text of `a`, `b` and `1`, an `Rc<str>` each.
    let streams = 7;
    let what = "a warm expansion, past its output and the macro's streams";
    let again = blaze_alloc_count::assert_at_most(output.allocations + streams, what, run);
    assert!(again == out);
}

/// The text from the first `from` to the next `to` after it.
fn between<'a>(text: &'a str, from: &str, to: &str) -> &'a str {
    let start = text.find(from).unwrap_or_else(|| panic!("no {from:?}"));
    let end = start + text[start..].find(to).unwrap_or_else(|| panic!("no {to:?} after {from:?}"));
    &text[start..end]
}

/// The imports the guest's exports alone call, which the bridge has no exports to call.
const EXPORTS_ONLY: [&str; 3] = ["output", "panicked", "retained"];

/// Each function `mod sys` of `text` declares, as `name(params) -> ret` without `unsafe` and
/// without a leading `_` on a parameter name.
fn declared(text: &str) -> Vec<String> {
    let sys = between(text, "\nmod sys {", "\n}\n");
    let mut all: Vec<String> = sys
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("pub fn ") || line.starts_with("pub unsafe fn "))
        .map(|line| {
            let line = line[..line.find(['{', ';']).unwrap_or(line.len())].trim();
            let line = line.replacen("pub unsafe fn ", "pub fn ", 1);
            let line = line.strip_prefix("pub fn ").expect("a function");
            let (name, rest) = line.split_once('(').expect("a function's parameters");
            let (params, ret) = rest.split_once(')').expect("a function's parameters end");
            let params: Vec<&str> = params
                .split(',')
                .map(|param| param.trim().trim_start_matches('_'))
                .filter(|param| !param.is_empty())
                .collect();
            format!("{name}({}){ret}", params.join(", "))
        })
        .collect();
    all.sort();
    all
}

#[test]
fn the_two_sys_modules_declare_the_same_functions() {
    let native = declared(include_str!("proc_macro/lib.rs"));
    let mut guest = declared(GUEST);
    let only = |it: &String| EXPORTS_ONLY.iter().any(|name| it.starts_with(&format!("{name}(")));
    assert_eq!(guest.iter().filter(|it| only(it)).count(), EXPORTS_ONLY.len());
    guest.retain(|it| !only(it));
    assert_eq!(native, guest);
    let api = include_str!("proc_macro/api.rs");
    for name in EXPORTS_ONLY {
        assert!(!api.contains(&format!("sys::{name}(")), "the API calls `sys::{name}`");
    }
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
