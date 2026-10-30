//! Defines [`SyntaxKind`] -- a fieldless enum of all possible syntactic
//! constructs of the Rust language.

#[rustfmt::skip]
mod generated;

use crate::Edition;

#[allow(unreachable_pub)]
pub use self::generated::SyntaxKind;

#[allow(unsafe_code)]
impl From<u16> for SyntaxKind {
    #[inline]
    fn from(d: u16) -> SyntaxKind {
        assert!(d <= (SyntaxKind::__LAST as u16));
        // SAFETY: `SyntaxKind` is `#[repr(u16)]` with no explicit discriminants
        // (`generated.rs`), so its values are exactly `0..=__LAST`, which the assert above
        // checks `d` is in. A value past `__LAST` would be an invalid enum, undefined behavior.
        unsafe { std::mem::transmute::<u16, SyntaxKind>(d) }
    }
}

impl From<SyntaxKind> for u16 {
    #[inline]
    fn from(k: SyntaxKind) -> u16 {
        k as u16
    }
}

impl SyntaxKind {
    #[inline]
    pub fn is_trivia(self) -> bool {
        matches!(self, SyntaxKind::WHITESPACE | SyntaxKind::COMMENT)
    }

    /// Returns true if this is an identifier or a keyword.
    #[inline]
    pub fn is_any_identifier(self) -> bool {
        // Assuming no edition removed keywords...
        self == SyntaxKind::IDENT || self.is_keyword(Edition::LATEST)
    }

    #[inline]
    pub fn is_l_delimiter(self) -> bool {
        matches!(self, SyntaxKind::L_CURLY | Self::L_PAREN | Self::L_BRACK)
    }

    #[inline]
    pub fn is_r_delimiter(self) -> bool {
        matches!(self, SyntaxKind::R_CURLY | Self::R_PAREN | Self::R_BRACK)
    }

    #[inline]
    pub fn is_delimiter(self) -> bool {
        self.is_l_delimiter() || self.is_r_delimiter()
    }
}
