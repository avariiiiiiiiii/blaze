//! `mbe` (short for Macro By Example) crate contains code for handling
//! `macro_rules` macros. It uses `TokenTree` (from `tt` package) as the
//! interface, although it contains some code to bridge `SyntaxNode`s and
//! `TokenTree`s as well!
//!
//! The tests for this functionality live in another crate:
//! `hir_def::macro_expansion_tests::mbe`.

#![cfg_attr(feature = "in-rust-tree", feature(rustc_private))]
#![forbid(unsafe_code)]

#[cfg(not(feature = "in-rust-tree"))]
extern crate ra_ap_rustc_lexer as rustc_lexer;
#[cfg(feature = "in-rust-tree")]
extern crate rustc_lexer;

mod expander;
mod macro_call_style;
mod parser;

#[cfg(test)]
mod benchmark;
#[cfg(test)]
mod differential;
#[cfg(test)]
mod tests;

use span::{Edition, Span, SyntaxContext};
use syntax_bridge::to_matcher_input_into;
use tt::DelimSpan;
use tt::iter::TtIter;

use std::cell::Cell;
use std::fmt;
use std::sync::Arc;

pub use crate::macro_call_style::{MacroCallStyle, MacroCallStyles};
use crate::parser::{MetaTemplate, MetaVarKind, Op};

pub use tt::{Delimiter, DelimiterKind, Punct};

#[derive(Debug, PartialEq, Eq, Clone)]
pub enum ParseError {
    UnexpectedToken(Box<str>),
    Expected(Box<str>),
    InvalidRepeat,
    RepetitionEmptyTokenTree,
}

impl ParseError {
    fn expected(e: &str) -> ParseError {
        ParseError::Expected(e.into())
    }

    fn unexpected(e: &str) -> ParseError {
        ParseError::UnexpectedToken(e.into())
    }
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParseError::UnexpectedToken(it) => f.write_str(it),
            ParseError::Expected(it) => f.write_str(it),
            ParseError::InvalidRepeat => f.write_str("invalid repeat"),
            ParseError::RepetitionEmptyTokenTree => f.write_str("empty token tree in repetition"),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub struct ExpandError {
    pub inner: Arc<(Span, ExpandErrorKind)>,
}
#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub enum ExpandErrorKind {
    BindingError(Box<Box<str>>),
    UnresolvedBinding(Box<Box<str>>),
    LeftoverTokens,
    LimitExceeded,
    NoMatchingRule,
    UnexpectedToken,
}

impl ExpandError {
    fn new(span: Span, kind: ExpandErrorKind) -> ExpandError {
        ExpandError { inner: Arc::new((span, kind)) }
    }
    fn binding_error(span: Span, e: impl Into<Box<str>>) -> ExpandError {
        ExpandError { inner: Arc::new((span, ExpandErrorKind::BindingError(Box::new(e.into())))) }
    }
}
impl fmt::Display for ExpandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.inner.1.fmt(f)
    }
}

impl fmt::Display for ExpandErrorKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ExpandErrorKind::NoMatchingRule => f.write_str("no rule matches input tokens"),
            ExpandErrorKind::UnexpectedToken => f.write_str("unexpected token in input"),
            ExpandErrorKind::BindingError(e) => f.write_str(e),
            ExpandErrorKind::UnresolvedBinding(binding) => {
                f.write_str("could not find binding ")?;
                f.write_str(binding)
            }
            ExpandErrorKind::LimitExceeded => f.write_str("Expand exceed limit"),
            ExpandErrorKind::LeftoverTokens => f.write_str("leftover tokens"),
        }
    }
}

// FIXME: Showing these errors could be nicer.
#[derive(Debug, PartialEq, Eq, Clone, Hash)]
pub enum CountError {
    OutOfBounds,
    Misplaced,
}

impl fmt::Display for CountError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CountError::OutOfBounds => f.write_str("${count} out of bounds"),
            CountError::Misplaced => f.write_str("${count} misplaced"),
        }
    }
}

/// Index of the matched macro arm on successful expansion.
pub type MatchedArmIndex = Option<u32>;

/// This struct contains AST for a single `macro_rules` definition. What might
/// be very confusing is that AST has almost exactly the same shape as
/// `tt::TokenTree`, but there's a crucial difference: in macro rules, `$ident`
/// and `$()*` have special meaning (see `Var` and `Repeat` data structures)
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeclarativeMacro {
    rules: Box<[Rule]>,
    err: Option<Box<ParseError>>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Rule {
    /// Is this a normal fn-like rule, an `attr()` rule, or a `derive()` rule?
    style: MacroCallStyle,
    lhs: MetaTemplate,
    rhs: MetaTemplate,
    /// `lhs` flattened for the matcher, once per definition.
    matcher: expander::Matcher,
}

impl DeclarativeMacro {
    pub fn from_err(err: ParseError) -> DeclarativeMacro {
        DeclarativeMacro { rules: Box::default(), err: Some(Box::new(err)) }
    }

    /// The old, `macro_rules! m {}` flavor.
    pub fn parse_macro_rules(
        tt: &tt::TopSubtree,
        ctx_edition: impl Copy + Fn(SyntaxContext) -> Edition,
    ) -> DeclarativeMacro {
        // Note: this parsing can be implemented using mbe machinery itself, by
        // matching against `$($lhs:tt => $rhs:tt);*` pattern, but implementing
        // manually seems easier.
        let mut src = tt.iter();
        let mut rules = Vec::new();
        let mut err = None;

        while !src.is_empty() {
            let rule = match Rule::parse(ctx_edition, &mut src) {
                Ok(it) => it,
                Err(e) => {
                    err = Some(Box::new(e));
                    break;
                }
            };
            rules.push(rule);
            if let Err(()) = src.expect_char(';') {
                if !src.is_empty() {
                    err = Some(Box::new(ParseError::expected("expected `;`")));
                }
                break;
            }
        }

        for Rule { lhs, .. } in &rules {
            if let Err(e) = validate(lhs) {
                err = Some(Box::new(e));
                break;
            }
        }

        DeclarativeMacro { rules: rules.into_boxed_slice(), err }
    }

    /// The new, unstable `macro m {}` flavor.
    pub fn parse_macro2(
        args: Option<&tt::TopSubtree>,
        body: &tt::TopSubtree,
        ctx_edition: impl Copy + Fn(SyntaxContext) -> Edition,
    ) -> DeclarativeMacro {
        let mut rules = Vec::new();
        let mut err = None;

        if let Some(args) = args {
            // The presence of an argument list means that this macro uses the
            // "simple" syntax, where the body is the RHS of a single rule.
            cov_mark::hit!(parse_macro_def_simple);

            let rule = (|| {
                let lhs = MetaTemplate::parse_pattern(ctx_edition, args.iter())?;
                let rhs = MetaTemplate::parse_template(ctx_edition, body.iter())?;

                // In the "simple" syntax, there is apparently no way to specify
                // that the single rule is an attribute or derive rule, so it
                // must be a function-like rule.
                Ok(crate::Rule::new(MacroCallStyle::FnLike, lhs, rhs))
            })();

            match rule {
                Ok(rule) => rules.push(rule),
                Err(e) => err = Some(Box::new(e)),
            }
        } else {
            // There was no top-level argument list, so this macro uses the
            // list-of-rules syntax, similar to `macro_rules!`.
            cov_mark::hit!(parse_macro_def_rules);
            let mut src = body.iter();
            while !src.is_empty() {
                let rule = match Rule::parse(ctx_edition, &mut src) {
                    Ok(it) => it,
                    Err(e) => {
                        err = Some(Box::new(e));
                        break;
                    }
                };
                rules.push(rule);
                if let Err(()) = src.expect_any_char(&[';', ',']) {
                    if !src.is_empty() {
                        err = Some(Box::new(ParseError::expected(
                            "expected `;` or `,` to delimit rules",
                        )));
                    }
                    break;
                }
            }
        }

        for Rule { lhs, .. } in &rules {
            if let Err(e) = validate(lhs) {
                err = Some(Box::new(e));
                break;
            }
        }

        DeclarativeMacro { rules: rules.into_boxed_slice(), err }
    }

    pub fn err(&self) -> Option<&ParseError> {
        self.err.as_deref()
    }

    pub fn num_rules(&self) -> usize {
        self.rules.len()
    }

    pub fn rule_styles(&self) -> MacroCallStyles {
        if self.rules.is_empty() {
            // No rules could be parsed, so fall back to assuming that this
            // is intended to be a function-like macro.
            MacroCallStyles::FN_LIKE
        } else {
            self.rules
                .iter()
                .map(|rule| MacroCallStyles::from(rule.style))
                .fold(MacroCallStyles::empty(), |a, b| a | b)
        }
    }

    pub fn expand(
        &self,
        db: &dyn salsa::Database,
        tt: &tt::TopSubtree,
        marker: impl Fn(&mut Span) + Copy,
        call_style: MacroCallStyle,
        call_site: Span,
    ) -> ExpandResult<(tt::TopSubtree, MatchedArmIndex)> {
        expander::expand_rules(db, &self.rules, tt, marker, call_style, call_site)
    }
}

impl Rule {
    fn parse(
        edition: impl Copy + Fn(SyntaxContext) -> Edition,
        src: &mut TtIter<'_>,
    ) -> Result<Self, ParseError> {
        // Parse an optional `attr()` or `derive()` prefix before the LHS pattern.
        let style = parser::parse_rule_style(src)?;

        let (_, lhs) =
            src.expect_subtree().map_err(|()| ParseError::expected("expected subtree"))?;
        src.expect_char('=').map_err(|()| ParseError::expected("expected `=`"))?;
        src.expect_char('>').map_err(|()| ParseError::expected("expected `>`"))?;
        let (_, rhs) =
            src.expect_subtree().map_err(|()| ParseError::expected("expected subtree"))?;

        let lhs = MetaTemplate::parse_pattern(edition, lhs)?;
        let rhs = MetaTemplate::parse_template(edition, rhs)?;

        Ok(crate::Rule::new(style, lhs, rhs))
    }

    fn new(style: MacroCallStyle, mut lhs: MetaTemplate, mut rhs: MetaTemplate) -> Rule {
        let binders = parser::resolve_binders(&mut lhs, &mut rhs);
        let matcher = expander::Matcher::new(&lhs, binders);
        Rule { style, lhs, rhs, matcher }
    }
}

fn validate(pattern: &MetaTemplate) -> Result<(), ParseError> {
    for op in pattern.iter() {
        match op {
            Op::Subtree { tokens, .. } => validate(tokens)?,
            Op::Repeat { tokens: subtree, separator, .. } => {
                // Checks that no repetition which could match an empty token
                // https://github.com/rust-lang/rust/blob/a58b1ed44f5e06976de2bdc4d7dc81c36a96934f/src/librustc_expand/mbe/macro_rules.rs#L558
                let lsh_is_empty_seq = separator.is_none() && subtree.iter().all(|child_op| {
                    match *child_op {
                        // vis is optional
                        Op::Var { kind: Some(kind), .. } => kind == MetaVarKind::Vis,
                        Op::Repeat {
                            kind: parser::RepeatKind::ZeroOrMore | parser::RepeatKind::ZeroOrOne,
                            ..
                        } => true,
                        _ => false,
                    }
                });
                if lsh_is_empty_seq {
                    return Err(ParseError::RepetitionEmptyTokenTree);
                }
                validate(subtree)?
            }
            _ => (),
        }
    }
    Ok(())
}

pub type ExpandResult<T> = ValueResult<T, ExpandError>;

#[derive(Debug, Clone, Eq, PartialEq, Hash)]
pub struct ValueResult<T, E> {
    pub value: T,
    pub err: Option<E>,
}

impl<T: Default, E> Default for ValueResult<T, E> {
    fn default() -> Self {
        Self { value: Default::default(), err: Default::default() }
    }
}

impl<T, E> ValueResult<T, E> {
    pub fn new(value: T, err: E) -> Self {
        Self { value, err: Some(err) }
    }

    pub fn ok(value: T) -> Self {
        Self { value, err: None }
    }

    pub fn only_err(err: E) -> Self
    where
        T: Default,
    {
        Self { value: Default::default(), err: Some(err) }
    }

    pub fn zip_val<U>(self, other: U) -> ValueResult<(T, U), E> {
        ValueResult { value: (self.value, other), err: self.err }
    }

    pub fn map<U>(self, f: impl FnOnce(T) -> U) -> ValueResult<U, E> {
        ValueResult { value: f(self.value), err: self.err }
    }

    pub fn as_ref(&self) -> ValueResult<&T, E>
    where
        E: Clone,
    {
        ValueResult { value: &self.value, err: self.err.clone() }
    }

    pub fn map_err<E2>(self, f: impl FnOnce(E) -> E2) -> ValueResult<T, E2> {
        ValueResult { value: self.value, err: self.err.map(f) }
    }

    pub fn result(self) -> Result<T, E> {
        self.err.map_or(Ok(self.value), Err)
    }
}

impl<T: Default, E> From<Result<T, E>> for ValueResult<T, E> {
    fn from(result: Result<T, E>) -> Self {
        result.map_or_else(Self::only_err, Self::ok)
    }
}

pub fn expect_fragment<'t>(
    db: &dyn salsa::Database,
    tt_iter: &mut TtIter<'t>,
    entry_point: ::parser::PrefixEntryPoint,
    delim_span: DelimSpan,
) -> ExpandResult<tt::TokenTreesView<'t>> {
    use ::parser;
    let buffer = tt_iter.remaining();
    let mut edition = |ctx: SyntaxContext| ctx.edition(db);
    // The parser reads only the first top-level trees of the level, doubled until it provably
    // read nothing past them: its position only moves forward, and it reads at most
    // `PARSER_LOOKAHEAD - 1` tokens past it (`Parser::nth` asserts `n <= 3`, and glued
    // punctuation looks two further). A fragment costs its own length, not the rest of the
    // level's, so `$($e:expr),*` is linear, not quadratic.
    let mut window_trees = FIRST_WINDOW_TREES;
    let mut parser_input =
        MATCHER_INPUT.take().unwrap_or_else(|| parser::Input::with_capacity(4 * window_trees));
    let tree_traversal = loop {
        let mut fork = tt_iter.clone();
        let savepoint = fork.savepoint();
        let mut taken = 0;
        while taken < window_trees && fork.next().is_some() {
            taken += 1;
        }
        let whole = fork.is_empty();
        to_matcher_input_into(fork.from_savepoint(savepoint), &mut edition, &mut parser_input);
        let output = entry_point.parse(&parser_input);
        if whole || consumed_input_tokens(&output) + PARSER_LOOKAHEAD <= parser_input.len() {
            break output;
        }
        window_trees *= 2;
    };
    if parser_input.len() <= KEEP_MATCHER_INPUT {
        MATCHER_INPUT.set(Some(parser_input));
    }
    // Over the whole level, so that `flat_advance_to` keeps its end.
    let mut cursor = buffer.cursor();
    let mut error = false;
    for step in tree_traversal.iter() {
        match step {
            parser::Step::Token { kind, mut n_input_tokens } => {
                if kind == ::parser::SyntaxKind::LIFETIME_IDENT {
                    n_input_tokens = 2;
                }
                for _ in 0..n_input_tokens {
                    cursor.skip_invisible();
                    cursor.bump_or_end();
                }
            }
            parser::Step::FloatSplit { .. } => {
                // FIXME: We need to split the tree properly here, but mutating the token trees
                // in the buffer is somewhat tricky to pull off.
                cursor.skip_invisible();
                cursor.bump_or_end();
            }
            parser::Step::Enter { .. } | parser::Step::Exit => (),
            parser::Step::Error { .. } => error = true,
        }
    }

    cursor.leave_invisible();
    let err = if error || !cursor.is_root() {
        Some(ExpandError::binding_error(
            buffer.cursor().token_tree().map_or(delim_span.close, |tt| tt.first_span()),
            format!("expected {entry_point:?}"),
        ))
    } else {
        None
    };

    while !cursor.is_root() {
        cursor.bump_or_end();
    }

    let res = cursor.crossed();
    tt_iter.flat_advance_to(&cursor);

    ExpandResult { value: res, err }
}

thread_local! {
    /// The input [`expect_fragment`] parses fragments from, kept for the next fragment: a
    /// matcher parses one a fragment, and none allocates.
    static MATCHER_INPUT: Cell<Option<::parser::Input>> = const { Cell::new(None) };
}

/// The most tokens an input kept for the next fragment holds; a larger one is freed.
const KEEP_MATCHER_INPUT: usize = 1 << 16;

/// How many tokens past its position the `parser` crate's parser may read, plus one.
const PARSER_LOOKAHEAD: usize = 6;

/// The top-level token trees [`expect_fragment`]'s first window holds.
const FIRST_WINDOW_TREES: usize = 8;

/// The parser input tokens a parse consumed: the position it ended at.
fn consumed_input_tokens(output: &::parser::Output) -> usize {
    output
        .iter()
        .map(|step| match step {
            ::parser::Step::Token { n_input_tokens, .. } => n_input_tokens as usize,
            ::parser::Step::FloatSplit { .. } => 1,
            ::parser::Step::Enter { .. } | ::parser::Step::Exit | ::parser::Step::Error { .. } => 0,
        })
        .sum()
}
