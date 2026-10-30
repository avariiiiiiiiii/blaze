//! Allocation budgets of hir-expand's hot reads: a `Name` from a symbol that exists, a
//! `ModPath` whose segment list exists, and span lookups, in order and scattered, each
//! allocating nothing; and a call's argument converted to tokens, a constant number. Each is
//! measured warm (its symbols and lists interned once before).

use intern::Symbol;
use span::{Edition, EditionedFileId, ErasedFileAstId, FileId, Span, SpanAnchor, SyntaxContext};
use syntax::{AstNode, SourceFile, TextRange, TextSize, ast};
use syntax_bridge::DocCommentDesugarMode;

use crate::{
    mod_path::{ModPath, PathKind},
    name::{AsName, Name},
    span_map::{ExpansionSpanMap, InOrder, RealSpanMap, SpanMap},
    tt,
};

fn span(range: TextRange) -> Span {
    Span {
        range,
        anchor: SpanAnchor {
            file_id: EditionedFileId::new(FileId::from_raw(7), Edition::CURRENT),
            ast_id: ErasedFileAstId::from_raw(0),
        },
        ctx: SyntaxContext::root(Edition::CURRENT),
    }
}

#[test]
fn a_name_of_an_existing_symbol_allocates_nothing() {
    let symbol = Symbol::intern("budget_name");
    let ident = tt::Ident {
        sym: symbol,
        span: span(TextRange::empty(TextSize::new(0))),
        is_raw: tt::IdentIsRaw::No,
    };
    let warm = (Name::new_root("budget_name"), Name::new_tuple_field(3), ident.as_name());
    let names = blaze_alloc_count::assert_at_most(0, "names of existing symbols", || {
        (
            Name::new_symbol_root(symbol),
            Name::new_root("budget_name"),
            Name::new_tuple_field(3),
            ident.as_name(),
        )
    });
    assert_eq!(names.0, warm.0);
    assert_eq!(names.1, warm.0);
    assert_eq!(names.2, warm.1);
    assert_eq!(names.3, warm.2);
}

#[test]
fn a_path_whose_segments_exist_allocates_nothing() {
    let [a, b, c] = ["budget_a", "budget_b", "budget_c"].map(Name::new_root);
    let warm = ModPath::from_segments(PathKind::Plain, [a, b, c]);
    let mut shorter = warm;
    shorter.pop_segment();
    let paths = blaze_alloc_count::assert_at_most(0, "paths whose segment lists exist", || {
        let mut path = ModPath::from_segments(PathKind::Plain, [a, b, c]);
        let popped = path.pop_segment();
        let mut again = path;
        again.push_segment(c);
        (path, popped, again, ModPath::from_kind(PathKind::Crate))
    });
    assert_eq!(paths.0, shorter);
    assert_eq!(paths.1, Some(c));
    assert_eq!(paths.2, warm);
    assert_eq!(paths.3.segments(), &[] as &[Name]);
}

#[test]
fn span_lookups_allocate_nothing() {
    let file_id = EditionedFileId::new(FileId::from_raw(7), Edition::CURRENT);
    let anchors: Box<[_]> = (0..32u32)
        .map(|i| (TextSize::new(i * 40), ErasedFileAstId::from_raw(i)))
        .collect();
    let real = RealSpanMap::from_file(file_id, anchors, TextSize::new(32 * 40));
    let mut expansion = ExpansionSpanMap::empty();
    let ranges: Vec<TextRange> =
        (0..320u32).map(|i| TextRange::new((i * 4).into(), (i * 4 + 3).into())).collect();
    for &range in &ranges {
        expansion.push(range.end(), span(range));
    }
    expansion.finish();
    for map in [SpanMap::RealSpanMap(&real), SpanMap::ExpansionSpanMap(&expansion)] {
        let expected: Vec<Span> = ranges.iter().map(|&range| map.span_for_range(range)).collect();
        let found = blaze_alloc_count::assert_at_most(0, "span lookups", || {
            let in_order = InOrder::new(map);
            let mut same = true;
            for (at, &range) in ranges.iter().enumerate() {
                same &= in_order.span_for_range(range) == expected[at];
                same &= map.span_for_range(range) == expected[at];
            }
            same
        });
        assert!(found, "the in-order lookup gives the plain lookup's spans");
    }
}

/// `macro_arg`'s conversion of a call's argument from syntax to tokens: a constant number of
/// allocations however long the argument is, since the builder is sized by the subtree's length.
#[test]
fn an_argument_costs_a_constant_number_of_allocations() {
    let used = |tokens: usize| {
        let text = format!("m!({});", "a + 1, ".repeat(tokens / 4));
        let file = SourceFile::parse(&text, Edition::CURRENT).tree();
        let arg = file.syntax().descendants().find_map(ast::TokenTree::cast).expect("the argument");
        let map = RealSpanMap::absolute(EditionedFileId::new(FileId::from_raw(7), Edition::CURRENT));
        let at = span(TextRange::empty(TextSize::new(0)));
        let run = || {
            syntax_bridge::syntax_node_to_token_tree(
                arg.syntax(),
                InOrder::new(SpanMap::RealSpanMap(&map)),
                at,
                DocCommentDesugarMode::Mbe,
            )
        };
        let warm = run();
        let (again, used) = blaze_alloc_count::measure(run);
        assert!(again == warm, "{tokens} tokens");
        used.allocations
    };
    let (small, big) = (used(16), used(1024));
    assert!(small <= 12, "an argument of 16 tokens: {small} allocations, budget 12");
    assert_eq!(big, small, "1024 tokens against 16: no allocation grows with the argument");
}
