use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use expect_test::expect;
use rustc_hash::{FxHashMap, FxHashSet};
use span::{Edition, Span};
use syntax::{AstNode, NodeOrToken, SyntaxNode, ast};
use test_utils::extract_annotations;
use tt::{Leaf, Punct, Spacing, TopSubtreeBuilder, TtElement, buffer::Cursor};

use crate::{
    DocCommentDesugarMode,
    dummy_test_span_utils::{DUMMY, DummyTestSpanMap},
    parse_to_token_tree_static_span, reference, syntax_node_to_token_tree,
    syntax_node_to_token_tree_modified, to_matcher_input, to_parser_input,
    token_tree_to_syntax_node,
};

fn check_punct_spacing(fixture: &str) {
    let source_file = ast::SourceFile::parse(fixture, span::Edition::CURRENT).ok().unwrap();
    let subtree = syntax_node_to_token_tree(
        source_file.syntax(),
        DummyTestSpanMap,
        DUMMY,
        DocCommentDesugarMode::Mbe,
    );
    let mut annotations: FxHashMap<_, _> = extract_annotations(fixture)
        .into_iter()
        .map(|(range, annotation)| {
            let spacing = match annotation.as_str() {
                "Alone" => Spacing::Alone,
                "Joint" => Spacing::Joint,
                a => panic!("unknown annotation: {a}"),
            };
            (range, spacing)
        })
        .collect();

    let mut cursor = Cursor::new(subtree.as_token_trees());
    while !cursor.eof() {
        while let Some(token_tree) = cursor.token_tree() {
            if let tt::TokenTree::Leaf(Leaf::Punct(Punct {
                spacing, span: Span { range, .. }, ..
            })) = token_tree
                && let Some(expected) = annotations.remove(&range)
            {
                assert_eq!(expected, spacing);
            }
            cursor.bump();
        }
        cursor.bump_or_end();
    }

    assert!(annotations.is_empty(), "unchecked annotations: {annotations:?}");
}

#[test]
fn punct_spacing() {
    check_punct_spacing(
        r#"
fn main() {
    0+0;
   //^ Alone
    0+(0);
   //^ Alone
    0<=0;
   //^ Joint
   // ^ Alone
    0<=(0);
   // ^ Alone
    a=0;
   //^ Alone
    a=(0);
   //^ Alone
    a+=0;
   //^ Joint
   // ^ Alone
    a+=(0);
   // ^ Alone
    a&&b;
   //^ Joint
   // ^ Alone
    a&&(b);
   // ^ Alone
    foo::bar;
   //  ^ Joint
   //   ^ Alone
    use foo::{bar,baz,};
   //       ^ Alone
   //            ^ Alone
   //                ^ Alone
    struct Struct<'a> {};
   //            ^ Joint
   //             ^ Joint
    Struct::<0>;
   //       ^ Alone
    Struct::<{0}>;
   //       ^ Alone
    ;;
  //^ Joint
  // ^ Alone
}
        "#,
    );
}

#[test]
fn scientific_notation_field_access_recovers() {
    let tt = parse_to_token_tree_static_span(
        span::Edition::CURRENT,
        DUMMY,
        r#"
fn main() {
    s.00E+10;
}
"#,
    )
    .unwrap();

    let (parse, _) = token_tree_to_syntax_node(&tt, parser::TopEntryPoint::SourceFile, &mut |_| {
        span::Edition::CURRENT
    });
    let parse = parse.cast::<ast::SourceFile>().unwrap();

    expect![[r#"
        SOURCE_FILE@0..19
          FN@0..19
            FN_KW@0..2 "fn"
            NAME@2..6
              IDENT@2..6 "main"
            PARAM_LIST@6..8
              L_PAREN@6..7 "("
              R_PAREN@7..8 ")"
            BLOCK_EXPR@8..19
              STMT_LIST@8..19
                L_CURLY@8..9 "{"
                EXPR_STMT@9..18
                  FIELD_EXPR@9..17
                    FIELD_EXPR@9..17
                      PATH_EXPR@9..10
                        PATH@9..10
                          PATH_SEGMENT@9..10
                            NAME_REF@9..10
                              IDENT@9..10 "s"
                      DOT@10..11 "."
                      ERROR@11..17
                        FLOAT_NUMBER@11..17 "00E+10"
                  SEMICOLON@17..18 ";"
                R_CURLY@18..19 "}"
        error 11..11: illegal float literal
    "#]]
    .assert_eq(&parse.debug_dump());
}

fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
    let mut entries: Vec<_> = std::fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("{}: {e}", dir.display()))
        .map(|it| it.expect("a directory entry").path())
        .collect();
    entries.sort();
    for path in entries {
        if path.is_dir() {
            rust_files(&path, out);
        } else if path.extension().is_some_and(|it| it == "rs") {
            out.push(path);
        }
    }
}

/// The parser's test data without errors and the `syntax` crate's own sources.
fn corpus() -> Vec<(PathBuf, Arc<str>)> {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut paths = Vec::new();
    for dir in ["parser/test_data/parser/ok", "parser/test_data/parser/inline/ok", "syntax/src"] {
        rust_files(&crates.join(dir), &mut paths);
    }
    assert!(paths.len() > 300, "the corpus is {} files", paths.len());
    paths
        .into_iter()
        .map(|path| {
            let text = std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            (path, Arc::from(text))
        })
        .collect()
}

fn token_tree(node: &SyntaxNode) -> tt::TopSubtree {
    syntax_node_to_token_tree(node, DummyTestSpanMap, DUMMY, DocCommentDesugarMode::Mbe)
}

#[test]
fn an_outline_has_the_token_trees_of_a_full_parse() {
    for (path, text) in corpus() {
        let path = path.display();
        let full = ast::SourceFile::parse_shared(text.clone(), Edition::CURRENT).syntax_node();
        let outline = ast::SourceFile::parse_outline(text, Edition::CURRENT).syntax_node();
        let mut in_full = FxHashMap::default();
        for node in full.descendants() {
            in_full.entry((node.kind(), node.text_range())).or_insert(node);
        }
        let in_full = |node: &SyntaxNode| {
            in_full
                .get(&(node.kind(), node.text_range()))
                .unwrap_or_else(|| panic!("{path}: {node:?} is not in the full parse"))
        };

        let (tree, _) = outline.flat().expect("an outline is a flat tree");
        let lazies: Vec<u32> = tree.lazies().collect();
        for &at in &lazies {
            let body = outline.flat_element(at).into_node().expect("a lazy element is a node");
            assert!(body.is_lazy(), "{path}: {body:?} was parsed before it was asked about");
            assert_eq!(token_tree(&body), token_tree(in_full(&body)), "{path}: {body:?}");
            assert!(tree.is_parsed(at), "{path}: {body:?}");
            let owner = body.parent().expect("a body has an owner");
            assert_eq!(token_tree(&owner), token_tree(in_full(&owner)), "{path}: {owner:?}");
        }

        let expected = token_tree(&full);
        assert_eq!(token_tree(&outline), expected, "{path}");
    }
}

#[test]
fn a_lazy_body_in_a_node_is_parsed_for_its_token_tree() {
    let text: Arc<str> = Arc::from("struct S;\nfn f() { let x = 1 + 2; g(x) }\n");
    let outline = ast::SourceFile::parse_outline(text.clone(), Edition::CURRENT).syntax_node();
    let (tree, _) = outline.flat().expect("an outline is a flat tree");
    let lazies: Vec<u32> = tree.lazies().collect();
    assert_eq!(lazies.len(), 1);
    assert!(!tree.is_parsed(lazies[0]));
    let full = ast::SourceFile::parse_shared(text, Edition::CURRENT).syntax_node();
    assert_eq!(token_tree(&outline), token_tree(&full));
    assert!(tree.is_parsed(lazies[0]));
}

#[test]
fn a_flat_tree_converts_as_the_cursor_does() {
    for (path, text) in corpus() {
        let path = path.display();
        let full = ast::SourceFile::parse_shared(text, Edition::CURRENT).syntax_node();
        assert!(full.flat().is_some(), "{path}: a full parse is a flat tree");
        for mode in [DocCommentDesugarMode::Mbe, DocCommentDesugarMode::ProcMacro] {
            let nodes = std::iter::once(full.clone())
                .chain(full.descendants().filter(|it| it.kind() == syntax::SyntaxKind::FN));
            for node in nodes {
                let flat = syntax_node_to_token_tree(&node, DummyTestSpanMap, DUMMY, mode);
                let cursor = syntax_node_to_token_tree_modified(
                    &node,
                    DummyTestSpanMap,
                    FxHashMap::default(),
                    FxHashSet::default(),
                    DUMMY,
                    mode,
                    |_, _| (true, Vec::new()),
                );
                assert_eq!(flat, cursor, "{path}: {node:?}");
                assert_eq!(flat.to_string(), cursor.to_string(), "{path}: {node:?}");
            }
        }
    }
}

/// What a closure gives, or that it panicked: the new and the old conversion must fail alike too.
fn caught<T>(f: impl FnOnce() -> T) -> Option<T> {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).ok()
}

/// Asserts `tree` converts to the same parser and matcher input as the old conversion did, for
/// each view the callers take, and parses to the same tree, errors and span map as each kind of
/// expansion.
fn assert_same_as_reference(
    tree: &tt::TopSubtree,
    edition: &mut dyn FnMut(span::SyntaxContext) -> Edition,
    what: &dyn Fn() -> String,
) {
    for view in [tree.view().strip_invisible(), tree.token_trees(), tree.as_token_trees()] {
        let new = caught(|| to_parser_input(view, &mut *edition));
        let old = caught(|| reference::to_parser_input(view, &mut *edition));
        match (&new, &old) {
            (Some(new), Some(old)) => {
                assert!(reference::same_input(new, old), "{}: the parser's input", what())
            }
            (None, None) => continue,
            _ => panic!("{}: one conversion panicked, the new one: {}", what(), new.is_none()),
        }
        let new = to_matcher_input(view, &mut *edition);
        let old = reference::to_matcher_input(view, &mut *edition);
        assert!(reference::same_input(&new, &old), "{}: the matcher's input", what());
    }
    use parser::TopEntryPoint::*;
    for entry in [MacroItems, MacroStmts, Expr, Pattern, Type, MetaItem] {
        let new = caught(|| {
            let (parse, map) = token_tree_to_syntax_node(tree, entry, &mut *edition);
            (parse.debug_dump(), map)
        });
        let old = caught(|| {
            let (parse, map) = reference::token_tree_to_syntax_node(tree, entry, &mut *edition);
            (parse.debug_dump(), map)
        });
        match (new, old) {
            (Some((new_tree, new_map)), Some((old_tree, old_map))) => {
                assert_eq!(new_tree, old_tree, "{}: parsed as {entry:?}", what());
                assert!(new_map == old_map, "{}: the span maps as {entry:?}", what());
            }
            (None, None) => (),
            (new, _) => {
                panic!("{}: one parse as {entry:?} panicked, the new: {}", what(), new.is_none())
            }
        }
    }
}

/// xorshift64*, for trees that are the same on every run.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    fn chance(&mut self, one_in: u64) -> bool {
        self.next() % one_in == 0
    }
}

const EDITIONS: [Edition; 4] =
    [Edition::Edition2015, Edition::Edition2018, Edition::Edition2021, Edition::Edition2024];

/// Copies `trees` into `out` with what expansions add: groups without delimiters around some
/// token trees (and some empty ones), as transcribed fragments are, and runs of tokens from the
/// root contexts of different editions, as tokens from different macros are. A lifetime's `'`
/// and its name stay in one group, as the lexer and every fragment keep them.
fn regroup(
    trees: tt::TokenTreesView<'_>,
    rng: &mut Rng,
    ctx: &mut usize,
    out: &mut TopSubtreeBuilder,
) {
    fn with_ctx(span: Span, rng: &mut Rng, ctx: &mut usize) -> Span {
        if rng.chance(6) {
            *ctx = (rng.next() % EDITIONS.len() as u64) as usize;
        }
        Span { ctx: span::SyntaxContext::root(EDITIONS[*ctx]), ..span }
    }
    let mut iter = trees.iter();
    while let Some(tree) = iter.next() {
        if rng.chance(16) {
            out.open(tt::DelimiterKind::Invisible, DUMMY);
            out.close(DUMMY);
        }
        let group = rng.chance(4);
        if group {
            out.open(tt::DelimiterKind::Invisible, with_ctx(DUMMY, rng, ctx));
        }
        let mut tree = Some(tree);
        while let Some(it) = tree.take() {
            match it {
                TtElement::Leaf(mut leaf) => {
                    let quote = matches!(&leaf, Leaf::Punct(Punct { char: '\'', .. }));
                    let span = match &mut leaf {
                        Leaf::Punct(it) => &mut it.span,
                        Leaf::Ident(it) => &mut it.span,
                        Leaf::Literal(it) => &mut it.span,
                    };
                    *span = with_ctx(*span, rng, ctx);
                    out.push(leaf);
                    if quote {
                        tree = iter.next();
                    }
                }
                TtElement::Subtree(subtree, inner) => {
                    out.open(subtree.delimiter.kind, with_ctx(subtree.delimiter.open, rng, ctx));
                    regroup(inner.remaining(), rng, ctx, out);
                    out.close(with_ctx(subtree.delimiter.close, rng, ctx));
                }
            }
        }
        if group {
            out.close(with_ctx(DUMMY, rng, ctx));
        }
    }
}

#[test]
fn parser_input_is_the_old_conversions() {
    let mut current = |_: span::SyntaxContext| Edition::CURRENT;
    let mut by_root = |ctx: span::SyntaxContext| {
        *EDITIONS
            .iter()
            .find(|&&it| span::SyntaxContext::root(it) == ctx)
            .expect("the trees here hold root contexts only")
    };
    for (file, (path, text)) in corpus().into_iter().enumerate() {
        let path = path.display();
        let full = ast::SourceFile::parse_shared(text, Edition::CURRENT).syntax_node();
        let nodes =
            std::iter::once(full.clone()).chain(full.descendants().filter(|it| it.kind() == syntax::SyntaxKind::FN));
        for (idx, node) in nodes.enumerate() {
            for mode in [DocCommentDesugarMode::Mbe, DocCommentDesugarMode::ProcMacro] {
                let tree = syntax_node_to_token_tree(&node, DummyTestSpanMap, DUMMY, mode);
                assert_same_as_reference(&tree, &mut current, &|| format!("{path}: {node:?}"));
            }
            let tree = token_tree(&node);
            let seed = (file as u64) << 20 | idx as u64;
            let mut rng = Rng(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
            let mut out = TopSubtreeBuilder::new(tree.top_subtree().delimiter);
            regroup(tree.token_trees(), &mut rng, &mut 0, &mut out);
            let regrouped = out.build();
            assert_same_as_reference(&regrouped, &mut by_root, &|| {
                format!("{path}: {node:?} regrouped\n{regrouped:?}")
            });
        }
    }
}

/// A mapper giving every token one span, as [`parse_to_token_tree_static_span`] does.
struct StaticSpanMap;

impl crate::SpanMapper for StaticSpanMap {
    fn span_for(&self, _: syntax::TextRange) -> Span {
        DUMMY
    }
}

/// The tokens and nodes of `node` but its trivia, in preorder, as kind and text.
fn structure(node: &SyntaxNode) -> Vec<(syntax::SyntaxKind, String)> {
    node.descendants_with_tokens()
        .filter_map(|it| match it {
            NodeOrToken::Node(it) => Some((it.kind(), String::new())),
            NodeOrToken::Token(it) => {
                (!it.kind().is_trivia()).then(|| (it.kind(), it.text().to_owned()))
            }
        })
        .collect()
}

/// Lexing a file into token trees gives what converting its syntax tree gives, and parsing those
/// tokens gives the file's syntax tree but its trivia: no step between the lexer's tokens, the
/// token trees and the parser changes a token.
#[test]
fn lexing_then_parsing_gives_the_files_tree() {
    let mut parsed = 0;
    for (path, text) in corpus() {
        let path = path.display();
        // A shebang and a frontmatter are only in the file.
        if text.starts_with("#!") || text.starts_with("---") {
            continue;
        }
        let full = ast::SourceFile::parse_shared(text.clone(), Edition::CURRENT).syntax_node();
        let lexed = parse_to_token_tree_static_span(Edition::CURRENT, DUMMY, &text)
            .unwrap_or_else(|| panic!("{path}: the corpus lexes"));
        let converted = syntax_node_to_token_tree(
            &full,
            StaticSpanMap,
            DUMMY,
            DocCommentDesugarMode::ProcMacro,
        );
        assert_eq!(lexed, converted, "{path}");
        // A doc comment is trivia in the file and an attribute in its tokens.
        let has_docs = full.descendants_with_tokens().any(|it| {
            it.as_token().is_some_and(|it| {
                it.kind() == syntax::SyntaxKind::COMMENT
                    && ["///", "//!", "/**", "/*!"].iter().any(|doc| it.text().starts_with(doc))
            })
        });
        if has_docs {
            continue;
        }
        let (parse, _) = token_tree_to_syntax_node(
            &lexed,
            parser::TopEntryPoint::SourceFile,
            &mut |_| Edition::CURRENT,
        );
        assert_eq!(structure(&parse.syntax_node()), structure(&full), "{path}");
        parsed += 1;
    }
    assert!(parsed > 200, "only {parsed} files parsed back");
}
