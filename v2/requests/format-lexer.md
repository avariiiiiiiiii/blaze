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
