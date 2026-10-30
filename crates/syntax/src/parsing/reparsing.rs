//! Implementation of incremental re-parsing.
//!
//! The smallest element holding the edit that can be parsed again alone is, and its new tree
//! is spliced into the old one ([`FlatTree::replace_subtree`]), the first of:
//!   - the token, when the edit keeps it one token of its kind (an identifier's letter);
//!   - in an outline, a `fn` body, lazy or not, which is a lazy node again;
//!   - the nearest `{ ... }` block or list [`Reparser::for_node`] parses;
//!   - an item at the top level of the file.
//!
//! Each is checked to parse alone as it does in the file: its new text lexes alone as it does
//! between its neighbours, its tokens balance, the tokens the parser may look at from outside
//! it are unchanged, and its parse is one node of its kind taking all of its text. When none
//! is, [`incremental_reparse`] is `None` and the caller parses the whole text.

use std::sync::Arc;

use parser::{Edition, LexedStr, Reparser};
use syntax_tree::{FlatTree, Language};

use crate::{
    AstNode, SyntaxError,
    SyntaxKind::{self, *},
    SyntaxTreeBuilder, T, TextRange, TextSize, ast,
    parsing::{build_node, lexer_errors},
    syntax_node::RustLanguage,
};

/// How many tokens into a node the parser may look from outside it: the node's first ones
/// must not change for the node to be parsed again alone.
const LOOKAHEAD: usize = 4;

/// How many tokens after a top-level item its parse is given, to stop where it stops in the
/// file.
const AFTER_ITEM: usize = 16;

/// The tree of `tree`'s text with `delete` replaced by `insert`, its errors (`errors`, those
/// of `tree`, merged with those of what was parsed again) and the range of the old text parsed
/// again. `outline` is the edition of an outline.
pub(crate) fn incremental_reparse(
    tree: &FlatTree,
    delete: TextRange,
    insert: &str,
    errors: impl IntoIterator<Item = SyntaxError>,
    edition: Edition,
    outline: Option<Edition>,
) -> Option<(FlatTree, Vec<SyntaxError>, TextRange)> {
    // An outline's lazy nodes are parsed in its edition: another one is a new parse.
    if outline.is_some_and(|it| it != edition) {
        return None;
    }
    let source = tree.source();
    let mut text = String::with_capacity(source.len() + insert.len());
    text.push_str(&source[..usize::from(delete.start())]);
    text.push_str(insert);
    text.push_str(&source[usize::from(delete.end())..]);
    let edit = Edit {
        tree,
        delete,
        insert,
        new_source: Arc::from(text),
        edition,
        outline: outline.is_some(),
    };
    let (tree, new_errors, range) = edit.token().or_else(|| edit.node())?;
    Some((tree, merge_errors(errors, new_errors, range, delete, insert), range))
}

struct Edit<'a> {
    tree: &'a FlatTree,
    delete: TextRange,
    insert: &'a str,
    new_source: Arc<str>,
    edition: Edition,
    outline: bool,
}

type Spliced = (FlatTree, Vec<SyntaxError>, TextRange);

impl Edit<'_> {
    fn kind(&self, at: u32) -> SyntaxKind {
        RustLanguage::kind_from_raw(self.tree.kind(at))
    }

    /// Whether `at` is a leaf: a token or a lazy node.
    fn is_leaf(&self, at: u32) -> bool {
        self.tree.is_token(at) || self.tree.is_lazy(at)
    }

    /// The text of the old `range`, which holds the edit, after it.
    fn new_text(&self, range: TextRange) -> &str {
        let len = range.len() + TextSize::of(self.insert) - self.delete.len();
        &self.new_source[TextRange::at(range.start(), len)]
    }

    fn token(&self) -> Option<Spliced> {
        let tree = self.tree;
        let at = covering(tree, self.delete);
        if !tree.is_token(at) {
            return None;
        }
        let kind = self.kind(at);
        if !matches!(kind, WHITESPACE | COMMENT | IDENT | STRING | BYTE_STRING | C_STRING) {
            return None;
        }
        let range = tree.range(at);
        let old_text = tree.token_text(at);
        if matches!(kind, WHITESPACE | COMMENT)
            // removing a new line may extend the previous token
            && old_text[self.delete - range.start()].contains('\n')
        {
            return None;
        }
        let new_text = self.new_text(range);
        let (new_kind, new_err) = LexedStr::single_token(self.edition, new_text)?;
        let contextual =
            |text: &str| SyntaxKind::from_contextual_keyword(text, self.edition).is_some();
        if new_kind != kind || (kind == IDENT && (contextual(old_text) || contextual(new_text))) {
            return None;
        }
        // Which comments an item takes (`n_attached_trivias`) depends on their style and on the
        // blank lines between them.
        if kind == COMMENT && comment_style(old_text) != comment_style(new_text) {
            return None;
        }
        let prev = (0..at).rev().find(|&it| self.is_leaf(it));
        let next = (at + 1..tree.len()).find(|&it| self.is_leaf(it));
        if kind == WHITESPACE
            && old_text.contains("\n\n") != new_text.contains("\n\n")
            && [prev, next].into_iter().flatten().any(|it| self.kind(it) == COMMENT)
        {
            return None;
        }

        // The edited token must not take the start of the next one, nor the previous one its
        // start: for source code `bruh"str"`, removing `ruh` makes `b"str"` one byte string.
        if let Some(next_char) = self.tree.source()[usize::from(range.end())..].chars().next() {
            let with_next = format!("{new_text}{next_char}");
            if LexedStr::single_token(self.edition, &with_next).is_some() {
                return None;
            }
        }
        if let Some(prev) = prev.filter(|&it| tree.is_token(it)) {
            let prev_text = tree.token_text(prev);
            let alone = LexedStr::new(self.edition, prev_text).len();
            let joint = format!("{prev_text}{new_text}");
            let joint = LexedStr::new(self.edition, &joint);
            if joint.len() != alone + 1 || joint.text(joint.len() - 1) != new_text {
                return None;
            }
        }

        let parent = tree.parent(at)?;
        let mut builder = SyntaxTreeBuilder::sourced(Arc::from(self.new_text(tree.range(parent))));
        let mut open = Vec::new();
        for it in parent..tree.end(parent) {
            while open.last().is_some_and(|&end| end <= it) {
                open.pop();
                builder.finish_node();
            }
            let kind = self.kind(it);
            if it == at {
                builder.token(kind, new_text);
            } else if tree.is_token(it) {
                builder.token(kind, tree.token_text(it));
            } else if tree.is_lazy(it) {
                builder.lazy(kind, tree.range(it).len());
            } else {
                builder.start_node(kind);
                open.push(tree.end(it));
            }
        }
        for _ in open {
            builder.finish_node();
        }
        let (with, _no_errors) = builder.finish_raw(self.edition);
        let token_range = TextRange::up_to(TextSize::of(new_text));
        let errors = new_err.into_iter().map(|msg| SyntaxError::new(msg, token_range)).collect();
        Some((tree.replace_subtree(parent, &with, self.new_source.clone()), errors, range))
    }

    /// The smallest node holding the edit that parses again alone, spliced.
    fn node(&self) -> Option<Spliced> {
        let tree = self.tree;
        let covering = covering(tree, self.delete);
        let start = match tree.is_token(covering) {
            true => tree.parent(covering)?,
            false => covering,
        };
        let ancestors: Vec<u32> = std::iter::successors(Some(start), |&it| tree.parent(it)).collect();
        // In an outline, what is in a `fn` body that is not lazy is parsed in full.
        let mut in_full = vec![!self.outline; ancestors.len()];
        let mut in_body = false;
        for (i, &it) in ancestors.iter().enumerate().rev() {
            in_full[i] |= in_body;
            in_body |= self.is_fn_body(it);
        }
        ancestors.iter().zip(in_full).find_map(|(&at, in_full)| {
            let kind = self.kind(at);
            if !in_full && self.is_fn_body(at) {
                return self.body(at);
            }
            let first_child = tree.children_with_tokens(at).next().map(|it| self.kind(it));
            let parent = tree.parent(at).map(|it| self.kind(it));
            if let Some(reparser) = Reparser::for_node(kind, first_child, parent) {
                return self.block(at, reparser, in_full);
            }
            if parent == Some(SOURCE_FILE) && ast::Item::can_cast(kind) {
                return self.item(at, in_full);
            }
            None
        })
    }

    fn is_fn_body(&self, at: u32) -> bool {
        self.kind(at) == BLOCK_EXPR && self.tree.parent(at).is_some_and(|it| self.kind(it) == FN)
    }

    /// A `fn` body of an outline, as a lazy node: one exactly when its tokens let the outline
    /// leave it unparsed (`braced`), which it did or the full parse of the body would have
    /// to be checked too.
    fn body(&self, at: u32) -> Option<Spliced> {
        let text = self.new_text(self.tree.range(at));
        let lexed = LexedStr::new(self.edition, text);
        if !braced(&lexed, self.edition)
            || !self.same_start(at, &lexed)
            || !self.lexes_alone(at, &lexed)
        {
            return None;
        }
        let mut builder = SyntaxTreeBuilder::sourced(Arc::from(text));
        builder.lazy(BLOCK_EXPR, TextSize::of(text));
        let (with, _no_errors) = builder.finish_raw(self.edition);
        self.splice(at, with, lexer_errors(&lexed, TextSize::new(0)).collect())
    }

    fn block(&self, at: u32, reparser: Reparser, in_full: bool) -> Option<Spliced> {
        let kind = self.kind(at);
        let text = self.new_text(self.tree.range(at));
        let lexed = LexedStr::new(self.edition, text);
        // An expression's parse looks into the block or list after it (a `{` after a path is
        // a struct literal's when an identifier and a `:` follow); a list of items, fields or
        // variants is its parent's last part.
        let looked_into = matches!(kind, BLOCK_EXPR | RECORD_EXPR_FIELD_LIST | MATCH_ARM_LIST);
        if !braced(&lexed, self.edition)
            || (looked_into && !self.same_start(at, &lexed))
            || !self.lexes_alone(at, &lexed)
        {
            return None;
        }
        let input = lexed.to_input(self.edition);
        let output = match in_full {
            true => reparser.parse(&input),
            false => reparser.parse_outline(&input),
        };
        let (with, errors) = build_node(Arc::from(text), &lexed, &output, self.edition, kind)?;
        self.splice(at, with, errors)
    }

    /// An item of the source file, parsed as the file's top level parses it: with the tokens
    /// after it, to stop where it stops in the file.
    fn item(&self, at: u32, in_full: bool) -> Option<Spliced> {
        let tree = self.tree;
        let kind = self.kind(at);
        let range = tree.range(at);
        let text = self.new_text(range);
        let lexed = LexedStr::new(self.edition, text);
        // Trivia after an item are its parent's; a parse of the item alone would take them.
        if lexed.is_empty()
            || lexed.kind(lexed.len() - 1).is_trivia()
            || !self.same_start(at, &lexed)
            || !self.lexes_alone(at, &lexed)
        {
            return None;
        }
        let mut seen = 0;
        let ahead = (tree.end(at)..tree.len())
            .filter(|&it| self.is_leaf(it))
            .find(|&it| {
                let kind = self.kind(it);
                seen += usize::from(!kind.is_trivia());
                seen >= AFTER_ITEM && !kind.is_trivia() && !matches!(kind, INT_NUMBER | T![.])
            })
            .map_or(tree.source().len(), |it| usize::from(tree.range(it).end()));
        // `lexes_alone` showed the lexer ends a token where the item ends.
        let ahead = LexedStr::new(self.edition, &tree.source()[usize::from(range.end())..ahead]);
        let input = lexed.to_input_followed_by(&ahead, self.edition);
        let output = match in_full {
            true => Reparser::top_level_item().parse(&input),
            false => Reparser::top_level_item().parse_outline(&input),
        };
        let (output, first, n_tokens) = output.into_first_node()?;
        let tokens = (0..lexed.len()).filter(|&it| !lexed.kind(it).is_trivia()).count();
        if first != kind || n_tokens != tokens {
            return None;
        }
        let (with, errors) = build_node(Arc::from(text), &lexed, &output, self.edition, kind)?;
        self.splice(at, with, errors)
    }

    fn splice(&self, at: u32, with: FlatTree, errors: Vec<SyntaxError>) -> Option<Spliced> {
        let tree = self.tree.replace_subtree(at, &with, self.new_source.clone());
        Some((tree, errors, self.tree.range(at)))
    }

    /// Whether node `at`'s first [`LOOKAHEAD`] tokens are those of `new`, its new text. When the
    /// edit starts past them and the two tokens after them, the old text is the same up to there
    /// and lexes to the same tokens (the lexer reads at most a character past a token to end
    /// it), so it is not lexed.
    fn same_start(&self, at: u32, new: &LexedStr<'_>) -> bool {
        let range = self.tree.range(at);
        let edit = usize::from(self.delete.start() - range.start());
        let last = (0..new.len()).filter(|&it| !new.kind(it).is_trivia()).nth(LOOKAHEAD - 1);
        if let Some(last) = last
            && last + 2 < new.len()
            && new.text_range(last + 2).end <= edit
        {
            return true;
        }
        let old = LexedStr::new(self.edition, &self.tree.source()[range]);
        first_tokens(&old) == first_tokens(new)
    }

    /// Whether `new`, the new text of node `at`, lexes alone as it does between the tokens
    /// around it: the lexing of the text from a token before it to a token after it is that of
    /// the three parts alone. The tokens bounding it are where the lexer's are: not pieces of
    /// a float literal the parser split (`x.0.1`), nor where a lexing starting there reads a
    /// shebang or front matter.
    fn lexes_alone(&self, at: u32, new: &LexedStr<'_>) -> bool {
        let (tree, source) = (self.tree, self.tree.source());
        let range = tree.range(at);
        let bound = |it: &u32| !matches!(self.kind(*it), INT_NUMBER | T![.] | T![#] | T![-] | WHITESPACE);
        // A lazy node bounds at its last token, a `}`, or its first, a `{`: its body is not
        // lexed again.
        let before = (0..at).rev().filter(|&it| self.is_leaf(it)).find(bound).map_or(
            TextSize::new(0),
            |it| match tree.is_lazy(it) {
                true => tree.range(it).end() - TextSize::of('}'),
                false => tree.range(it).start(),
            },
        );
        let after = (tree.end(at)..tree.len()).filter(|&it| self.is_leaf(it)).find(bound).map_or(
            TextSize::of(&**source),
            |it| match tree.is_lazy(it) {
                true => tree.range(it).start() + TextSize::of('{'),
                false => tree.range(it).end(),
            },
        );
        let before = &source[TextRange::new(before, range.start())];
        let after = &source[TextRange::new(range.end(), after)];
        let joint = format!("{before}{}{after}", new.as_str());
        let joint = LexedStr::new(self.edition, &joint);
        let (before, after) =
            (LexedStr::new(self.edition, before), LexedStr::new(self.edition, after));
        let alone = [&before, new, &after].into_iter().flat_map(token_lengths);
        alone.eq(token_lengths(&joint))
    }
}

/// Whether a comment reads as outer and as inner to `parser`'s attaching of comments to items.
fn comment_style(text: &str) -> (bool, bool) {
    let outer = !(text.starts_with("////") || text.starts_with("/***"))
        && (text.starts_with("///") || text.starts_with("/**"));
    (outer, text.starts_with("//!") || text.starts_with("/*!"))
}

fn token_lengths(lexed: &LexedStr<'_>) -> Vec<(SyntaxKind, usize)> {
    (0..lexed.len()).map(|it| (lexed.kind(it), lexed.text_len(it))).collect()
}

/// The first [`LOOKAHEAD`] tokens of `lexed` that are not trivia.
fn first_tokens<'a>(lexed: &'a LexedStr<'_>) -> Vec<(SyntaxKind, &'a str)> {
    (0..lexed.len())
        .filter(|&it| !lexed.kind(it).is_trivia())
        .take(LOOKAHEAD)
        .map(|it| (lexed.kind(it), lexed.text(it)))
        .collect()
}

/// Whether `lexed` is a `{ ... }` that parses to its last `}`: its braces balance and it holds
/// none of the tokens that make a block's parse take a brace as a plain token
/// ([`parser::lazy_block_len`]).
fn braced(lexed: &LexedStr<'_>, edition: Edition) -> bool {
    if lexed.is_empty() || lexed.kind(0) != T!['{'] || lexed.kind(lexed.len() - 1) != T!['}'] {
        return false;
    }
    let input = lexed.to_input(edition);
    parser::lazy_block_len(&input, 0) == u32::try_from(input.len()).ok()
}

/// The smallest element of `tree` whose range holds `range`, never in a lazy node's part:
/// for an empty range between two elements, the one before it.
fn covering(tree: &FlatTree, range: TextRange) -> u32 {
    let mut at = 0;
    loop {
        if tree.is_token(at) || tree.is_lazy(at) {
            return at;
        }
        let (mut overlapping, mut before, mut first) = (None, None, None);
        for child in tree.children_with_tokens(at) {
            first = first.or(Some(child));
            let child_range = tree.range(child);
            if child_range.end() <= range.start() {
                before = Some(child);
            } else if range.end() <= child_range.start() {
                break;
            } else {
                overlapping = Some(child);
                break;
            }
        }
        match overlapping.or(before).or(first) {
            Some(child) if tree.range(child).contains_range(range) => at = child,
            _ => return at,
        }
    }
}

fn merge_errors(
    old_errors: impl IntoIterator<Item = SyntaxError>,
    new_errors: Vec<SyntaxError>,
    range_before_reparse: TextRange,
    delete: TextRange,
    insert: &str,
) -> Vec<SyntaxError> {
    let mut res = Vec::new();

    for old_err in old_errors {
        let old_err_range = old_err.range();
        if old_err_range.end() <= range_before_reparse.start() {
            res.push(old_err);
        } else if old_err_range.start() >= range_before_reparse.end() {
            let inserted_len = TextSize::of(insert);
            res.push(old_err.with_range((old_err_range + inserted_len) - delete.len()));
            // Note: extra parens are intentional to prevent uint underflow, HWAB (here was a bug)
        }
    }
    res.extend(new_errors.into_iter().map(|new_err| {
        // fighting borrow checker with a variable ;)
        let offsetted_range = new_err.range() + range_before_reparse.start();
        new_err.with_range(offsetted_range)
    }));
    res
}

#[cfg(test)]
mod tests {
    use std::ops::Range;

    use parser::Edition;
    use test_utils::{assert_eq_text, extract_range};

    use super::*;
    use crate::{Parse, SourceFile, Tree};

    fn do_check(before: &str, replace_with: &str, reparsed_len: u32) {
        let (range, before) = extract_range(before);
        let after = {
            let mut after = before.clone();
            after.replace_range(Range::<usize>::from(range), replace_with);
            after
        };

        let fully_reparsed = SourceFile::parse(&after, Edition::CURRENT);
        let incrementally_reparsed: Parse<SourceFile> = {
            let before = SourceFile::parse(&before, Edition::CURRENT);
            let (tree, new_errors, range) = incremental_reparse(
                before.tree.as_ref().unwrap().flat(),
                range,
                replace_with,
                before.errors.as_deref().unwrap_or_default().iter().cloned(),
                Edition::CURRENT,
                None,
            )
            .unwrap();
            assert_eq!(range.len(), reparsed_len.into(), "reparsed fragment has wrong length");
            Parse::new(Tree::new(std::sync::Arc::new(tree)), new_errors)
        };

        assert_eq_text!(
            &format!("{:#?}", fully_reparsed.tree().syntax()),
            &format!("{:#?}", incrementally_reparsed.tree().syntax()),
        );
        assert_eq!(fully_reparsed.errors(), incrementally_reparsed.errors());

        let outline = SourceFile::parse_outline(before.into(), Edition::CURRENT);
        check_same(
            &outline.reparse(range, replace_with, Edition::CURRENT),
            &SourceFile::parse_outline(after.into(), Edition::CURRENT),
        );
    }

    /// `reparsed` and `parsed` are the same parse: their trees, which of their nodes are lazy,
    /// and their errors (in another order, see `merge_errors`).
    fn check_same(reparsed: &Parse<SourceFile>, parsed: &Parse<SourceFile>) {
        let lazies = |parse: &Parse<SourceFile>| {
            let tree = parse.tree.as_ref().unwrap().flat();
            tree.lazies().map(|it| tree.range(it)).collect::<Vec<_>>()
        };
        assert_eq!(lazies(reparsed), lazies(parsed), "different lazy nodes");
        assert_eq_text!(
            &format!("{:#?}", parsed.tree().syntax()),
            &format!("{:#?}", reparsed.tree().syntax()),
        );
        let errors = |parse: &Parse<SourceFile>| {
            let mut errors: Vec<_> = parse
                .errors_with_bodies()
                .into_iter()
                .map(|it| (it.range().start(), it.range().end(), it.to_string()))
                .collect();
            errors.sort();
            errors
        };
        assert_eq!(errors(reparsed), errors(parsed));
    }

    #[test] // FIXME: some test here actually test token reparsing
    fn reparse_block_tests() {
        do_check(
            r"
fn foo() {
    let x = foo + $0bar$0
}
",
            "baz",
            3,
        );
        do_check(
            r"
fn foo() {
    let x = foo$0 + bar$0
}
",
            "baz",
            25,
        );
        do_check(
            r"
struct Foo {
    f: foo$0$0
}
",
            ",\n    g: (),",
            14,
        );
        do_check(
            r"
fn foo {
    let;
    1 + 1;
    $092$0;
}
",
            "62",
            31, // FIXME: reparse only int literal here
        );
        do_check(
            r"
mod foo {
    fn $0$0
}
",
            "bar",
            11,
        );

        do_check(
            r"
trait Foo {
    type $0Foo$0;
}
",
            "Output",
            3,
        );
        do_check(
            r"
impl IntoIterator<Item=i32> for Foo {
    f$0$0
}
",
            "n next(",
            9,
        );
        do_check(r"use a::b::{foo,$0,bar$0};", "baz", 10);
        do_check(
            r"
pub enum A {
    Foo$0$0
}
",
            "\nBar;\n",
            11,
        );
        do_check(
            r"
foo!{a, b$0$0 d}
",
            ", c[3]",
            8,
        );
        do_check(
            r"
fn foo() {
    vec![$0$0]
}
",
            "123",
            14,
        );
        do_check(
            r"
extern {
    fn$0;$0
}
",
            " exit(code: c_int)",
            11,
        );
    }

    #[test]
    fn reparse_token_tests() {
        do_check(
            r"$0$0
fn foo() -> i32 { 1 }
",
            "\n\n\n   \n",
            1,
        );
        do_check(
            r"
fn foo() -> $0$0 {}
",
            "  \n",
            2,
        );
        do_check(
            r"
fn $0foo$0() -> i32 { 1 }
",
            "bar",
            3,
        );
        do_check(
            r"
fn foo$0$0foo() {  }
",
            "bar",
            6,
        );
        do_check(
            r"
fn foo /* $0$0 */ () {}
",
            "some comment",
            6,
        );
        do_check(
            r"
fn baz $0$0 () {}
",
            "    \t\t\n\n",
            2,
        );
        do_check(
            r"
fn baz $0$0 () {}
",
            "    \t\t\n\n",
            2,
        );
        do_check(
            r#"
fn -> &str { "Hello$0$0" }
"#,
            ", world",
            7,
        );
        do_check(
            r#"
fn -> &str { // "Hello$0$0"
"#,
            ", world",
            10,
        );
        do_check(
            r##"
fn -> &str { r#"Hello$0$0"#
"##,
            ", world",
            10,
        );
        do_check(
            r"
#[derive($0Copy$0)]
enum Foo {

}
",
            "Clone",
            4,
        );
    }

    #[test]
    fn reparse_str_token_with_error_unchanged() {
        do_check(r#""$0Unclosed$0 string literal"#, "Still unclosed", 24);
    }

    #[test]
    fn reparse_str_token_with_error_fixed() {
        do_check(r#""unterminated$0$0"#, "\"", 13);
    }

    #[test]
    fn reparse_block_with_error_in_middle_unchanged() {
        do_check(
            r#"fn main() {
                if {}
                32 + 4$0$0
                return
                if {}
            }"#,
            "23",
            105,
        )
    }

    #[test]
    fn reparse_block_with_error_in_middle_fixed() {
        do_check(
            r#"fn main() {
                if {}
                32 + 4$0$0
                return
                if {}
            }"#,
            ";",
            105,
        )
    }

    #[test]
    fn reparse_lazy_body() {
        let before = "fn f() {\n    let x = 1;\n}\nfn g() {}\n";
        let after = "fn f() {\n    let x = 2 + 3;\n}\nfn g() {}\n";
        let outline = SourceFile::parse_outline(before.into(), Edition::CURRENT);
        let at = TextRange::at(TextSize::new(21), TextSize::new(1));
        assert_eq!(&before[at], "1");
        let (tree, errors, range) = incremental_reparse(
            outline.tree.as_ref().unwrap().flat(),
            at,
            "2 + 3",
            [],
            Edition::CURRENT,
            Some(Edition::CURRENT),
        )
        .unwrap();
        assert_eq!(range, TextRange::new(TextSize::new(7), TextSize::new(25)), "f's body");
        let reparsed = Parse::with_outline(
            Tree::new(std::sync::Arc::new(tree)),
            errors,
            Some(Edition::CURRENT),
        );
        let parsed = SourceFile::parse_outline(after.into(), Edition::CURRENT);
        assert_eq!(parsed.tree.as_ref().unwrap().flat().lazies().count(), 2);
        check_same(&reparsed, &parsed);
    }

    /// A small deterministic generator, for the edits of [`reparse_random_edits`].
    struct Rng(u64);

    impl Rng {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 % n.max(1) as u64) as usize
        }
    }

    const INSERTS: &[&str] = &[
        "", " ", "\n", "\n\n", "{", "}", "(", ")", "[", "]", ";", ",", "x", "_", "#", "#[a]",
        "<", ">", "1.0", ".0", "0", "\"", "'", "r#", "//", "/// d\n", "/*", "*/", "let x = 1;",
        "fn f() {}", "fn g() { 1 }", "impl S {}", "struct S { a: u8 }", "mod m {}", "=>", "::",
        "unsafe", "async", "union", "macro_rules! m {}",
    ];

    /// Random edits of real files, each reparse the parse of the edited text: in full and as an
    /// outline.
    #[test]
    fn reparse_random_edits() {
        let root = test_utils::project_root().into_std_path_buf();
        let mut files = Vec::new();
        for dir in ["crates/syntax/src", "crates/parser/src"] {
            collect_rs(&root.join(dir), &mut files);
        }
        for dir in ["ok", "inline/ok"] {
            collect_rs(&root.join("crates/parser/test_data/parser").join(dir), &mut files);
        }
        files.sort();
        assert!(files.len() > 100, "the sources and test data of syntax and parser");
        let mut rng = Rng(0x2545_f491_4f6c_dd1d);
        for file in &files {
            let text = std::fs::read_to_string(file).unwrap();
            let edits = if text.len() > 20_000 { 2 } else { 6 };
            let full = SourceFile::parse(&text, Edition::CURRENT);
            let outline = SourceFile::parse_outline(text.as_str().into(), Edition::CURRENT);
            for _ in 0..edits {
                let (delete, insert) = random_edit(&mut rng, &text);
                let mut after = text.clone();
                after.replace_range(Range::<usize>::from(delete), &insert);
                let check = || {
                    let reparsed = full.reparse(delete, &insert, Edition::CURRENT);
                    let parsed = SourceFile::parse(&after, Edition::CURRENT);
                    assert_eq_text!(
                        &format!("{:#?}", parsed.tree().syntax()),
                        &format!("{:#?}", reparsed.tree().syntax()),
                    );
                    let reparsed = outline.reparse(delete, &insert, Edition::CURRENT);
                    let parsed = SourceFile::parse_outline(after.as_str().into(), Edition::CURRENT);
                    check_same(&reparsed, &parsed);
                };
                if std::panic::catch_unwind(std::panic::AssertUnwindSafe(check)).is_err() {
                    panic!("{}: {delete:?} replaced by {insert:?}", file.display());
                }
            }
        }
    }

    fn random_edit(rng: &mut Rng, text: &str) -> (TextRange, String) {
        let boundary = |mut at: usize| {
            while !text.is_char_boundary(at) {
                at -= 1;
            }
            at
        };
        let start = boundary(rng.below(text.len() + 1));
        let end = boundary((start + rng.below(12)).min(text.len())).max(start);
        let insert = match rng.below(4) {
            0 => {
                let from = boundary(rng.below(text.len() + 1));
                let to = boundary((from + rng.below(24)).min(text.len())).max(from);
                text[from..to].to_owned()
            }
            _ => INSERTS[rng.below(INSERTS.len())].to_owned(),
        };
        let delete = TextRange::new(start.try_into().unwrap(), end.try_into().unwrap());
        (delete, insert)
    }

    fn collect_rs(dir: &std::path::Path, files: &mut Vec<std::path::PathBuf>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                collect_rs(&path, files);
            } else if path.extension().is_some_and(|it| it == "rs") {
                files.push(path);
            }
        }
    }
}
