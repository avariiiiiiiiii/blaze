//! Outlines: a parse that leaves every `fn` body unparsed.
//!
//! In an outline ([`crate::TopEntryPoint::parse_outline`]) a `fn` item's body block is skipped
//! as the balanced `{ ... }` of its tokens and emitted as one lazy node
//! ([`crate::OutlineStep::Lazy`]) of kind `BLOCK_EXPR`. Parsing the block's text alone with
//! [`crate::Reparser::for_node`] of `BLOCK_EXPR` then gives exactly the subtree a full parse
//! makes there, because the grammar takes each `{` and `}` in a block as the delimiters they
//! match, as rustc's token trees do. A few error recoveries take a brace as a plain token
//! instead, all of them in code rustc rejects; [`lazy_block_len`] refuses a body holding one
//! it can see from the tokens, and that `fn` is parsed in full in the outline, which makes the
//! same tree. The one it cannot see, a brace where a generic parameter list expects a
//! parameter (`fn f<T, {>()`) in a `fn` nested in a body, makes the body's part fail its check
//! when parsed; outlines are for the files of dependency crates, which rustc builds.

use smallvec::SmallVec;

use crate::{
    SyntaxKind::{EOF, IDENT, OUTER_DOC_COMMENT},
    T, Tokens,
};

/// The number of tokens of the balanced `{ ... }` starting at token `start` of `input`, when an
/// outline may leave it unparsed: its braces balance before the input ends, and it holds none
/// of the tokens whose error recovery takes a brace as a plain token: an outer attribute or a
/// doc comment right before a `}` (the statement expected after it takes the `}`), a `#` or a
/// `<` right before a brace (the expression or generic parameter expected there takes it) and
/// `builtin #` (whose `asm` operands' recovery takes any token).
pub fn lazy_block_len<'t>(input: impl Into<Tokens<'t>>, start: usize) -> Option<u32> {
    let input = input.into();
    if input.kind(start) != T!['{'] {
        return None;
    }
    let mut depth = 0usize;
    let mut brackets = 0usize;
    // The bracket depths at which the outer attributes being read started.
    let mut attributes: SmallVec<[usize; 4]> = SmallVec::new();
    let mut after_attribute = false;
    let mut at = start;
    loop {
        let kind = input.kind(at);
        let next = input.kind(at + 1);
        let attribute_before = std::mem::replace(&mut after_attribute, false);
        match kind {
            EOF => return None,
            T!['{'] => depth += 1,
            T!['}'] => {
                if attribute_before {
                    return None;
                }
                depth -= 1;
                if depth == 0 {
                    return u32::try_from(at + 1 - start).ok();
                }
            }
            T!['['] => brackets += 1,
            T![']'] => {
                brackets = brackets.saturating_sub(1);
                if attributes.last() == Some(&brackets) {
                    attributes.pop();
                    after_attribute = true;
                }
            }
            T![#] if next == T!['['] => attributes.push(brackets),
            T![#] | T![<] if matches!(next, T!['{'] | T!['}']) => return None,
            OUTER_DOC_COMMENT => after_attribute = true,
            IDENT if input.contextual_kind(at) == T![builtin] && next == T![#] => return None,
            _ => (),
        }
        at += 1;
    }
}
