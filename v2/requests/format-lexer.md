# Requests from format-lexer

## format-lexer: lower `LabeledLoopInBreak` and the lexer's new messages as rustc gives them (diagnostics, `crates/blaze-diagnostics/src/lower/parse.rs`)

What, in `File::parser_error` and `File::lexer_error`:

1. The parser error "parentheses are required around this expression to avoid confusion with a
   labeled break expression" (`crates/parser/src/grammar/expressions/atom.rs` `break_expr`) is at
   the end of the labeled value of an unlabeled `BREAK_EXPR`. rustc's `LabeledLoopInBreak`: span
   the value expression (the `BREAK_EXPR`'s expression child, which ends at the error's offset),
   the machine-applicable multipart suggestion "wrap the expression in parentheses" (`(` before,
   `)` after), and recovered (`true`): rustc goes on parsing. Today it falls into the `_` arm,
   which spans the next token and gives up. Needed by
   `tests/lints/syntax/early/validation.rs::a_labeled_break_of_an_unlabeled_loop_is_linted_after_the_parse_errors`.
2. The lexer now gives rustc's text: "prefix `z` is unknown" (from 2021 only; `'foo` for a
   lifetime), "`x` cannot be a raw identifier", "`x` cannot be a raw lifetime" and "character
   literal may only contain one codepoint" (after a raw lifetime from 2021). They reach the
   pass-through arm already; what's missing is rustc's note "prefixed identifiers and literals
   are reserved since Rust 2021", the label "unknown prefix" and `UnknownPrefixSugg` (`rb` → `br`,
   `rc` → `cr`, the string-with-single-quotes and whitespace suggestions,
   `report_unknown_prefix`), and the `'r#a'` error's span, which in rustc runs through the
   following `'`.

Why: so blaze prints what rustc prints for these programs (the lexer and the grammar give the
tokens and the tree; the diagnostic shape is the lowering's).

> forwarded to diagnostics (told by tmux).

## format-lexer: a branch-free keyword probe (format-grammar, `crates/parser/src/syntax_kind/keywords.rs`)

What: `SyntaxKind::from_keyword(text, edition)` runs on every identifier the lexer makes (30% of
tokens). Today it costs about 15 instructions with a length filter and a perfect hash. A probe of
about 8 would do: build a key from the length and the first two bytes, use it to index the
table, then one masked 8-byte compare against the stored keyword, then the edition check on the
entry. Every strict keyword (`keywords::strict`, the only lookup the lexer makes) is at most 8
bytes long. The contextual table needn't change. It must give the same answer as today for every
string and edition. A test over all keywords, every edition and a corpus of identifiers would
show it.

Why: the lexer's speed of light (`docs/v2/plan/lanes/format/speed-of-light.md`, "The lexer",
item 3). It lowers the lexer's per-token minimum from about 13.5 instructions to about 11.5.

> forwarded to format-grammar (told by tmux).
