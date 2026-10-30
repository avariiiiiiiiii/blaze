//! See [`Input`].

use edition::Edition;

use crate::SyntaxKind;

/// Input for the parser -- a sequence of tokens.
///
/// As of now, parser doesn't have access to the *text* of the tokens, and makes
/// decisions based solely on their classification. Unlike `LexerToken`, the
/// `Tokens` doesn't include whitespace and comments. Main input to the parser.
///
/// One `u32` word a token, the lexer's word family ([`crate::LexedStr`], the flat tree): the
/// kind in the low [`KIND_BITS`] bits, then the contextual kind, the edition, whether the token
/// is joint to the next and whether it starts a fragment a macro transcribed. 4 bytes a token
/// against the 5.25 of five columns, and every question about a token is one load.
pub struct Input {
    words: Vec<u32>,
}

const KIND_BITS: u32 = 10;
const KIND_MASK: u32 = (1 << KIND_BITS) - 1;
const CONTEXTUAL_SHIFT: u32 = KIND_BITS;
const EDITION_SHIFT: u32 = 2 * KIND_BITS;
const EDITION_MASK: u32 = 0b111;
const JOINT: u32 = 1 << (EDITION_SHIFT + 3);
const FRAGMENT: u32 = JOINT << 1;

const _: () = assert!((SyntaxKind::__LAST as u32) <= KIND_MASK, "a kind fits in KIND_BITS");
const _: () = assert!((Edition::LATEST as u32) <= EDITION_MASK, "an edition fits in 3 bits");

/// The word of a token of `kind` and `contextual_kind` in `edition`, neither joint nor a
/// fragment's start.
#[inline]
const fn word(kind: SyntaxKind, contextual_kind: SyntaxKind, edition: Edition) -> u32 {
    kind as u16 as u32
        | (contextual_kind as u16 as u32) << CONTEXTUAL_SHIFT
        | (edition as u32) << EDITION_SHIFT
}

/// `pub` impl used by callers to create `Tokens`.
impl Input {
    #[inline]
    pub fn with_capacity(capacity: usize) -> Self {
        Self { words: Vec::with_capacity(capacity) }
    }

    /// Empties the input, keeping its buffer for the next one.
    #[inline]
    pub fn clear(&mut self) {
        self.words.clear();
    }

    /// Makes room for `additional` more tokens.
    #[inline]
    pub fn reserve(&mut self, additional: usize) {
        self.words.reserve(additional);
    }

    #[inline]
    pub fn push(&mut self, kind: SyntaxKind, edition: Edition) {
        self.words.push(word(kind, SyntaxKind::EOF, edition));
    }
    #[inline]
    pub fn push_ident(&mut self, contextual_kind: SyntaxKind, edition: Edition) {
        self.words.push(word(SyntaxKind::IDENT, contextual_kind, edition));
    }
    /// Sets jointness for the last token we've pushed.
    ///
    /// This is a separate API rather than an argument to the `push` to make it
    /// convenient both for textual and mbe tokens. With text, you know whether
    /// the *previous* token was joint, with mbe, you know whether the *current*
    /// one is joint. This API allows for styles of usage:
    ///
    /// ```ignore
    /// // In text:
    /// tokens.was_joint(prev_joint);
    /// tokens.push(curr);
    ///
    /// // In MBE:
    /// token.push(curr);
    /// tokens.push(curr_joint)
    /// ```
    #[inline]
    pub fn was_joint(&mut self) {
        let last = self.words.last_mut().expect("`was_joint` after a token");
        *last |= JOINT;
    }
    /// Marks token `n` as the first of a fragment that a macro transcribed (an `$e:expr`): as
    /// to rustc, the fragment is one operand, which nothing before it takes as its operator,
    /// call or index.
    #[inline]
    pub fn mark_fragment_start(&mut self, n: usize) {
        self.words[n] |= FRAGMENT;
    }
}

/// pub(crate) impl used by the parser to consume `Tokens`.
impl Input {
    #[inline]
    pub(crate) fn kind(&self, idx: usize) -> SyntaxKind {
        match self.words.get(idx) {
            Some(&word) => SyntaxKind::from((word & KIND_MASK) as u16),
            None => SyntaxKind::EOF,
        }
    }
    #[inline]
    pub(crate) fn contextual_kind(&self, idx: usize) -> SyntaxKind {
        match self.words.get(idx) {
            Some(&word) => SyntaxKind::from(((word >> CONTEXTUAL_SHIFT) & KIND_MASK) as u16),
            None => SyntaxKind::EOF,
        }
    }
    #[inline]
    pub(crate) fn edition(&self, idx: usize) -> Edition {
        Edition::from_u32((self.words[idx] >> EDITION_SHIFT) & EDITION_MASK)
    }
    #[inline]
    pub(crate) fn is_joint(&self, n: usize) -> bool {
        self.words.get(n).is_some_and(|&word| word & JOINT != 0)
    }
    #[inline]
    pub(crate) fn is_fragment_start(&self, n: usize) -> bool {
        self.words.get(n).is_some_and(|&word| word & FRAGMENT != 0)
    }
}

impl Input {
    #[inline]
    pub fn len(&self) -> usize {
        self.words.len()
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.words.is_empty()
    }

    /// Token `i` as the parser sees it: its kind, contextual kind, edition, whether it is
    /// joint and whether it starts a fragment. For comparing inputs made two ways.
    pub fn token(&self, i: usize) -> (SyntaxKind, SyntaxKind, Edition, bool, bool) {
        (
            self.kind(i),
            self.contextual_kind(i),
            self.edition(i),
            self.is_joint(i),
            self.is_fragment_start(i),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every field reads back what was written, for every kind, edition and flag, and no
    /// field disturbs another.
    #[test]
    fn words_read_back_their_fields() {
        let mut input = Input::with_capacity(0);
        let mut expected = Vec::new();
        for raw in 0..=(SyntaxKind::__LAST as u16) {
            let kind = SyntaxKind::from(raw);
            for edition in Edition::iter() {
                let contextual = SyntaxKind::from(SyntaxKind::__LAST as u16 - raw);
                let i = input.len();
                let ident = raw % 3 == 0;
                if ident {
                    input.push_ident(contextual, edition);
                } else {
                    input.push(kind, edition);
                }
                let joint = raw % 2 == 0;
                if joint {
                    input.was_joint();
                }
                let fragment = raw % 5 == 0;
                if fragment {
                    input.mark_fragment_start(i);
                }
                expected.push(match ident {
                    true => (SyntaxKind::IDENT, contextual, edition, joint, fragment),
                    false => (kind, SyntaxKind::EOF, edition, joint, fragment),
                });
            }
        }
        for (i, expected) in expected.iter().enumerate() {
            assert_eq!(input.token(i), *expected, "token {i}");
        }
        assert_eq!(input.kind(input.len()), SyntaxKind::EOF);
        assert_eq!(input.contextual_kind(input.len() + 7), SyntaxKind::EOF);
        assert!(!input.is_joint(input.len()) && !input.is_fragment_start(input.len()));
        input.clear();
        assert!(input.is_empty());
    }
}
