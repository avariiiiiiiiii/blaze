//! This module takes a (parsed) definition of `macro_rules` invocation, a
//! `tt::TokenTree` representing an argument of macro invocation, and produces a
//! `tt::TokenTree` for the result of the expansion.

mod matcher;
#[cfg(test)]
pub(crate) mod model;
mod transcriber;

use base_db::Db;
use blaze_mem::frame::Frame;
use span::Span;

pub(crate) use self::matcher::Matcher;
use crate::{
    ExpandError, ExpandErrorKind, ExpandResult, MacroCallStyle, MatchedArmIndex,
    parser::{Binder, MetaVarKind},
};

/// An absent index in the matcher's and the bindings' `u32` columns.
const NONE: u32 = u32::MAX;

pub(crate) fn expand_rules(
    db: &Db,
    rules: &[crate::Rule],
    input: &tt::TopSubtree,
    marker: impl Fn(&mut Span) + Copy,
    call_style: MacroCallStyle,
    call_site: Span,
) -> ExpandResult<(tt::TopSubtree, MatchedArmIndex)> {
    blaze_mem::frame::task(blaze_mem::Tag::Expand, |frame| {
        expand_rules_in(frame, db, rules, input, marker, call_style, call_site)
    })
}

/// [`expand_rules`] with every match and transcription temporary in `frame`.
fn expand_rules_in<'t>(
    frame: &'t Frame,
    db: &Db,
    rules: &[crate::Rule],
    input: &'t tt::TopSubtree,
    marker: impl Fn(&mut Span) + Copy,
    call_style: MacroCallStyle,
    call_site: Span,
) -> ExpandResult<(tt::TopSubtree, MatchedArmIndex)> {
    // One set of buffers for every rule tried, and two matches swapped between the best so far
    // and the one being made, so a rule after the first allocates only where it outgrows them.
    let mut buffers = matcher::Buffers::new_in(frame);
    let mut scratch = matcher::Match::new_in(frame);
    let mut best: Option<(matcher::Match<'t>, usize)> = None;
    for (idx, rule) in rules.iter().enumerate() {
        // Skip any rules that aren't relevant to the call style (fn-like/attr/derive).
        if call_style != rule.style {
            continue;
        }

        matcher::match_(db, &rule.matcher, input, &mut buffers, &mut scratch);

        if scratch.err.is_none() {
            // If we find a rule that applies without errors, we're done.
            // Unconditionally returning the transcription here makes the
            // `test_repeat_bad_var` test fail.
            let ExpandResult { value, err: transcribe_err } =
                transcriber::transcribe(&rule.rhs, &scratch.bindings, frame, marker, call_site);
            if transcribe_err.is_none() {
                return ExpandResult::ok((value, Some(idx as u32)));
            }
        }
        // Use the rule if we matched more tokens, or bound variables count
        match &mut best {
            Some((prev, prev_idx)) => {
                if (scratch.unmatched_tts, -(scratch.bound_count as i32))
                    < (prev.unmatched_tts, -(prev.bound_count as i32))
                {
                    std::mem::swap(prev, &mut scratch);
                    *prev_idx = idx;
                }
            }
            None => {
                best = Some((std::mem::replace(&mut scratch, matcher::Match::new_in(frame)), idx))
            }
        }
    }
    if let Some((match_, idx)) = best {
        // if we got here, there was no match without errors
        let rule = &rules[idx];
        let ExpandResult { value, err: transcribe_err } =
            transcriber::transcribe(&rule.rhs, &match_.bindings, frame, marker, call_site);
        // What rustc says when no rule matches; its ambiguity errors are the matcher's own.
        let match_err = match_.err.map(|err| {
            matcher::match_failure(db, frame, rules, input, call_style).unwrap_or(err)
        });
        ExpandResult { value: (value, idx.try_into().ok()), err: match_err.or(transcribe_err) }
    } else {
        ExpandResult::new(
            (tt::TopSubtree::empty(tt::DelimSpan::from_single(call_site)), None),
            ExpandError::new(call_site, ExpandErrorKind::NoMatchingRule),
        )
    }
}

/// The actual algorithm for expansion is not too hard, but is pretty tricky.
/// `Bindings` structure is the key to understanding what we are doing here.
///
/// On the high level, it stores mapping from meta variables to the bits of
/// syntax it should be substituted with. For example, if `$e:expr` is matched
/// with `1 + 1` by macro_rules, the `Binding` will store `$e -> 1 + 1`.
///
/// The tricky bit is dealing with repetitions (`$()*`). Consider this example:
///
/// ```not_rust
/// macro_rules! foo {
///     ($($ i:ident $($ e:expr),*);*) => {
///         $(fn $ i() { $($ e);*; })*
///     }
/// }
/// foo! { foo 1,2,3; bar 4,5,6 }
/// ```
///
/// Here, the `$i` meta variable is matched first with `foo` and then with
/// `bar`, and `$e` is matched in turn with `1`, `2`, `3`, `4`, `5`, `6`.
///
/// To represent such "multi-mappings", we map variables not to values, but to
/// *lists* of values or other lists (that is, to the trees). For the above
/// example, the bindings would store
///
/// ```not_rust
/// i -> [foo, bar]
/// e -> [[1, 2, 3], [4, 5, 6]]
/// ```
///
/// The matcher builds the trees once a match is over ([`matcher::Buffers`]). The transcriber
/// substitutes them, keeping a `nesting` stack of indices which says which occurrence it takes
/// at each level; it pushes to the stack when it enters a repetition.
///
/// A variable is its [`Binder`], numbered when the macro was defined, so a lookup indexes
/// `roots` and never hashes a name. The trees are flat: `nodes` (12 bytes each) with each
/// `Nested` node's iterations a CSR range of `children`; a step down a level is one index.
#[derive(Debug)]
struct Bindings<'a> {
    /// Per binder: its tree, an index into `nodes`, or [`NONE`] when the match bound nothing
    /// to it.
    roots: Vec<u32, &'a Frame>,
    nodes: Vec<Binding, &'a Frame>,
    children: Vec<u32, &'a Frame>,
    fragments: Vec<Fragment<'a>, &'a Frame>,
}

#[derive(Debug, Clone, Copy)]
enum Binding {
    /// An index into `Bindings::fragments`.
    Fragment(u32),
    /// The iterations of a repetition: `Bindings::children[start..start + len]`.
    Nested {
        start: u32,
        len: u32,
    },
    Empty,
    Missing(MetaVarKind),
}

const _: () = assert!(size_of::<Binding>() == 12);

impl<'a> Bindings<'a> {
    fn new_in(frame: &'a Frame) -> Self {
        Bindings {
            roots: Vec::new_in(frame),
            nodes: Vec::new_in(frame),
            children: Vec::new_in(frame),
            fragments: Vec::new_in(frame),
        }
    }

    fn clear(&mut self) {
        self.roots.clear();
        self.nodes.clear();
        self.children.clear();
        self.fragments.clear();
    }

    fn root(&self, binder: Binder) -> Option<Binding> {
        let node = *self.roots.get(binder as usize)?;
        (node != NONE).then(|| self.nodes[node as usize])
    }

    /// Iteration `idx` of a `Binding::Nested { start, len }`.
    fn iteration(&self, start: u32, len: u32, idx: usize) -> Option<Binding> {
        (idx < len as usize).then(|| self.nodes[self.children[start as usize + idx] as usize])
    }

    fn fragment(&self, idx: u32) -> &Fragment<'a> {
        &self.fragments[idx as usize]
    }

    /// The token trees of all the fragments bound: what the transcriber reserves room for with
    /// the template's own.
    fn token_trees(&self) -> usize {
        self.fragments
            .iter()
            .map(|fragment| match fragment {
                Fragment::Empty => 0,
                Fragment::Tokens { tree, .. } => tree.len(),
                Fragment::Expr(it) | Fragment::Path(it) | Fragment::Ty(it) => it.len(),
                Fragment::TokensOwned(it) => it.as_token_trees().len(),
            })
            .sum()
    }

    /// The bindings under `binding`, each leaf counting one: how a match ranks among the
    /// rules' (`Match::bound_count`).
    fn bound(&self, binding: Binding) -> usize {
        match binding {
            Binding::Nested { start, len } => {
                let range = start as usize..(start + len) as usize;
                self.children[range].iter().map(|&it| self.bound(self.nodes[it as usize])).sum()
            }
            Binding::Fragment(_) | Binding::Empty | Binding::Missing(_) => 1,
        }
    }
}

#[derive(Debug, Default, Clone)]
enum Fragment<'a> {
    #[default]
    Empty,
    /// token fragments are just copy-pasted into the output
    Tokens {
        tree: tt::TokenTreesView<'a>,
        origin: TokensOrigin,
    },
    /// Expr ast fragments are surrounded with `()` on transcription to preserve precedence.
    /// Note that this impl is different from the one currently in `rustc` --
    /// `rustc` doesn't translate fragments into token trees at all.
    ///
    /// At one point in time, we tried to use "fake" delimiters here à la
    /// proc-macro delimiter=none. As we later discovered, "none" delimiters are
    /// tricky to handle in the parser, and rustc doesn't handle those either.
    ///
    /// The span of the outer delimiters is marked on transcription.
    Expr(tt::TokenTreesView<'a>),
    /// There are roughly two types of paths: paths in expression context, where a
    /// separator `::` between an identifier and its following generic argument list
    /// is mandatory, and paths in type context, where `::` can be omitted.
    ///
    /// Unlike rustc, we need to transform the parsed fragments back into tokens
    /// during transcription. When the matched path fragment is a type-context path
    /// and is trasncribed as an expression-context path, verbatim transcription
    /// would cause a syntax error. We need to fix it up just before transcribing;
    /// see `transcriber::fix_up_and_push_path_tt()`.
    Path(tt::TokenTreesView<'a>),
    /// A type is one thing to rustc wherever it is written, and tokens to us. `1 as $ty << n`
    /// would read `u32 <<` as the start of generic arguments: the type is parenthesized on
    /// transcription when a `<` follows it.
    Ty(tt::TokenTreesView<'a>),
    TokensOwned(tt::TopSubtree),
}

impl Fragment<'_> {
    fn is_empty(&self) -> bool {
        match self {
            Fragment::Empty => true,
            Fragment::Tokens { tree, .. } => tree.len() == 0,
            Fragment::Expr(it) => it.len() == 0,
            Fragment::Path(it) | Fragment::Ty(it) => it.len() == 0,
            Fragment::TokensOwned(_) => false, // A `TopSubtree` is never empty
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TokensOrigin {
    Raw,
    Ast,
}
