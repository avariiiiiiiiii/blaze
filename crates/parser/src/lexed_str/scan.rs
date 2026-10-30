//! The lexer's branch-light front: byte classes 64 at a time, and the tokens they decide.
//!
//! [`Classes`] is three bitmaps over the text, made in one pass with the widest vector unit the
//! CPU has (AVX-512BW, AVX2 or NEON, chosen at run time; every path gives the bitmaps the scalar
//! one gives): which bytes are ASCII identifier bytes (`[A-Za-z0-9_]`), ASCII whitespace
//! (`\t\n\x0B\x0C\r `) and `\n`. That costs 24 bytes per 64 of text, for the lexing only.
//!
//! [`token`] lexes the token at an offset from them when the bytes alone decide it the way
//! `rustc_lexer` does: whitespace and identifier runs are one `trailing_zeros` per 64 bytes, a
//! line comment's end is the next newline bit, and punctuation and plain decimal integers are a
//! byte or two of lookahead. That is most tokens of Rust source. A token that starts or ends
//! where only `rustc_lexer`'s Unicode tables or its literal grammar can say (a quote, a raw or
//! prefixed string, a block comment, a byte of 0x80 or more next to the token) gives `None`, and
//! the caller asks `rustc_lexer` for that token. Both give the same `(kind, length)` wherever
//! `token` gives one, which the tests check at every offset of every test file; so this is
//! dispatch by token class, not a second lexer with its own rules.
//!
//! One of the crate's two unsafe places (docs/v2/plan/design/unsafe.md; the other is
//! `SyntaxKind::from(u16)`): the vector loads and the
//! `target_feature` calls are unsafe. The safe version is [`blocks_scalar`], a table lookup and
//! shift per byte, 64 per block; the vector paths do a block in a few compares. Every path is
//! checked against it on every test file (`every_path_classifies_as_the_scalar_one`).

use rustc_lexer::{Base, DocStyle, LiteralKind, TokenKind};

const IDENT: usize = 0;
const SPACE: usize = 1;
const NEWLINE: usize = 2;

/// The byte classes of a text: for each 64 bytes, a bit per byte in each of [`IDENT`],
/// [`SPACE`] and [`NEWLINE`]. Bits past the text's end are clear in the first two and set in
/// the last, so every run stops at the end. 24 bytes per 64 of text; a lookup is one load.
pub(super) struct Classes {
    blocks: Vec<[u64; 3]>,
}

const _: () = assert!(size_of::<[u64; 3]>() == 24, "24 bytes of classes per 64 of text");

/// Bit 0: an ASCII identifier byte; bit 1: ASCII whitespace; bit 2: `\n`.
const CLASS: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut b = 0;
    while b < 256 {
        let c = b as u8;
        if c.is_ascii_alphanumeric() || c == b'_' {
            table[b] |= 1;
        }
        if matches!(c, b'\t' | b'\n' | 0x0B | 0x0C | b'\r' | b' ') {
            table[b] |= 2;
        }
        if c == b'\n' {
            table[b] |= 4;
        }
        b += 1;
    }
    table
};

/// The classes of 64 bytes or fewer, one byte at a time.
fn block_scalar(bytes: &[u8]) -> [u64; 3] {
    debug_assert!(bytes.len() <= 64);
    let mut out = [0u64; 3];
    for (i, &b) in bytes.iter().enumerate() {
        let class = CLASS[b as usize] as u64;
        out[IDENT] |= (class & 1) << i;
        out[SPACE] |= ((class >> 1) & 1) << i;
        out[NEWLINE] |= ((class >> 2) & 1) << i;
    }
    out
}

/// The classes of every whole 64 bytes of `bytes`, appended to `out`.
type Blocks = fn(&[u8], &mut Vec<[u64; 3]>);

fn blocks_scalar(bytes: &[u8], out: &mut Vec<[u64; 3]>) {
    out.extend(bytes.chunks_exact(64).map(block_scalar));
}

/// The fastest [`Blocks`] this CPU has.
fn blocks() -> Blocks {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx512bw") {
            fn avx512(bytes: &[u8], out: &mut Vec<[u64; 3]>) {
                // SAFETY: chosen only after `avx512bw` was detected.
                unsafe { blocks_avx512(bytes, out) }
            }
            return avx512;
        }
        if is_x86_feature_detected!("avx2") {
            fn avx2(bytes: &[u8], out: &mut Vec<[u64; 3]>) {
                // SAFETY: chosen only after `avx2` was detected.
                unsafe { blocks_avx2(bytes, out) }
            }
            return avx2;
        }
    }
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("neon") {
        fn neon(bytes: &[u8], out: &mut Vec<[u64; 3]>) {
            // SAFETY: chosen only after `neon` was detected.
            unsafe { blocks_neon(bytes, out) }
        }
        return neon;
    }
    blocks_scalar
}

/// Every [`Blocks`] this CPU can run, the scalar one first, for the differential tests.
#[cfg(test)]
pub(super) fn paths() -> Vec<(&'static str, Blocks)> {
    let mut out: Vec<(&'static str, Blocks)> = vec![("scalar", blocks_scalar)];
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx512bw") {
            fn avx512(bytes: &[u8], out: &mut Vec<[u64; 3]>) {
                // SAFETY: listed only after `avx512bw` was detected.
                unsafe { blocks_avx512(bytes, out) }
            }
            out.push(("avx512bw", avx512));
        }
        if is_x86_feature_detected!("avx2") {
            fn avx2(bytes: &[u8], out: &mut Vec<[u64; 3]>) {
                // SAFETY: listed only after `avx2` was detected.
                unsafe { blocks_avx2(bytes, out) }
            }
            out.push(("avx2", avx2));
        }
    }
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("neon") {
        fn neon(bytes: &[u8], out: &mut Vec<[u64; 3]>) {
            // SAFETY: listed only after `neon` was detected.
            unsafe { blocks_neon(bytes, out) }
        }
        out.push(("neon", neon));
    }
    out
}

/// [`blocks_scalar`] on AVX-512BW, 64 bytes per compare.
///
/// # Safety
///
/// The CPU has `avx512bw` (callers check `is_x86_feature_detected!`); without it the
/// instructions fault.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx512bw")]
unsafe fn blocks_avx512(bytes: &[u8], out: &mut Vec<[u64; 3]>) {
    use std::arch::x86_64::*;

    let case = _mm512_set1_epi8(0x20);
    let a = _mm512_set1_epi8(b'a' as i8);
    let letters = _mm512_set1_epi8(25);
    let zero = _mm512_set1_epi8(b'0' as i8);
    let digits = _mm512_set1_epi8(9);
    let underscore = _mm512_set1_epi8(b'_' as i8);
    let tab = _mm512_set1_epi8(b'\t' as i8);
    let controls = _mm512_set1_epi8(4);
    let space = _mm512_set1_epi8(b' ' as i8);
    let newline = _mm512_set1_epi8(b'\n' as i8);
    for chunk in bytes.chunks_exact(64) {
        // SAFETY: `chunks_exact(64)` makes `chunk` 64 readable bytes, exactly the load's width,
        // and `loadu` has no alignment requirement; a shorter chunk would read past the slice.
        let v = unsafe { _mm512_loadu_si512(chunk.as_ptr().cast()) };
        let alpha =
            _mm512_cmple_epu8_mask(_mm512_sub_epi8(_mm512_or_si512(v, case), a), letters);
        let digit = _mm512_cmple_epu8_mask(_mm512_sub_epi8(v, zero), digits);
        let ident = alpha | digit | _mm512_cmpeq_epi8_mask(v, underscore);
        let blank = _mm512_cmple_epu8_mask(_mm512_sub_epi8(v, tab), controls)
            | _mm512_cmpeq_epi8_mask(v, space);
        out.push([ident, blank, _mm512_cmpeq_epi8_mask(v, newline)]);
    }
}

/// `x <= k` per unsigned byte, as 0xFF or 0.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
fn le_avx2(
    x: std::arch::x86_64::__m256i,
    k: std::arch::x86_64::__m256i,
) -> std::arch::x86_64::__m256i {
    use std::arch::x86_64::*;
    _mm256_cmpeq_epi8(_mm256_min_epu8(x, k), x)
}

/// The three classes of 32 bytes.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
fn half_avx2(v: std::arch::x86_64::__m256i) -> [u64; 3] {
    use std::arch::x86_64::*;
    let alpha = le_avx2(
        _mm256_sub_epi8(_mm256_or_si256(v, _mm256_set1_epi8(0x20)), _mm256_set1_epi8(b'a' as i8)),
        _mm256_set1_epi8(25),
    );
    let digit = le_avx2(_mm256_sub_epi8(v, _mm256_set1_epi8(b'0' as i8)), _mm256_set1_epi8(9));
    let underscore = _mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'_' as i8));
    let ident = _mm256_or_si256(_mm256_or_si256(alpha, digit), underscore);
    let control = le_avx2(_mm256_sub_epi8(v, _mm256_set1_epi8(b'\t' as i8)), _mm256_set1_epi8(4));
    let blank = _mm256_or_si256(control, _mm256_cmpeq_epi8(v, _mm256_set1_epi8(b' ' as i8)));
    let newline = _mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'\n' as i8));
    [
        _mm256_movemask_epi8(ident) as u32 as u64,
        _mm256_movemask_epi8(blank) as u32 as u64,
        _mm256_movemask_epi8(newline) as u32 as u64,
    ]
}

/// [`blocks_scalar`] on AVX2, 32 bytes per compare.
///
/// # Safety
///
/// The CPU has `avx2` (callers check `is_x86_feature_detected!`); without it the instructions
/// fault.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
unsafe fn blocks_avx2(bytes: &[u8], out: &mut Vec<[u64; 3]>) {
    use std::arch::x86_64::*;
    for chunk in bytes.chunks_exact(64) {
        let (low, high) = chunk.split_at(32);
        // SAFETY: `low` is 32 readable bytes (`chunks_exact(64)` split in half), the load's
        // width, and `loadu` has no alignment requirement.
        let lo = unsafe { _mm256_loadu_si256(low.as_ptr().cast()) };
        // SAFETY: as for `low`: `high` is the other 32 bytes of the chunk.
        let hi = unsafe { _mm256_loadu_si256(high.as_ptr().cast()) };
        let (lo, hi) = (half_avx2(lo), half_avx2(hi));
        out.push([lo[0] | hi[0] << 32, lo[1] | hi[1] << 32, lo[2] | hi[2] << 32]);
    }
}

/// The bits of four 16-byte masks of 0xFF or 0, byte `i` of the 64 as bit `i`.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[inline]
fn movemask_neon(
    m: [std::arch::aarch64::uint8x16_t; 4],
    weights: std::arch::aarch64::uint8x16_t,
) -> u64 {
    use std::arch::aarch64::*;
    let ab = vpaddq_u8(vandq_u8(m[0], weights), vandq_u8(m[1], weights));
    let cd = vpaddq_u8(vandq_u8(m[2], weights), vandq_u8(m[3], weights));
    let abcd = vpaddq_u8(ab, cd);
    let abcd = vpaddq_u8(abcd, abcd);
    vgetq_lane_u64::<0>(vreinterpretq_u64_u8(abcd))
}

/// The three classes of 16 bytes, as 0xFF or 0 per byte.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[inline]
fn quarter_neon(v: std::arch::aarch64::uint8x16_t) -> [std::arch::aarch64::uint8x16_t; 3] {
    use std::arch::aarch64::*;
    let lower = vorrq_u8(v, vdupq_n_u8(0x20));
    let alpha = vcleq_u8(vsubq_u8(lower, vdupq_n_u8(b'a')), vdupq_n_u8(25));
    let digit = vcleq_u8(vsubq_u8(v, vdupq_n_u8(b'0')), vdupq_n_u8(9));
    let ident = vorrq_u8(vorrq_u8(alpha, digit), vceqq_u8(v, vdupq_n_u8(b'_')));
    let control = vcleq_u8(vsubq_u8(v, vdupq_n_u8(b'\t')), vdupq_n_u8(4));
    let blank = vorrq_u8(control, vceqq_u8(v, vdupq_n_u8(b' ')));
    [ident, blank, vceqq_u8(v, vdupq_n_u8(b'\n'))]
}

/// 16 bytes of `bytes` from `at` as a vector.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[inline]
fn load_neon(bytes: &[u8], at: usize) -> std::arch::aarch64::uint8x16_t {
    let lane: &[u8; 16] = bytes[at..at + 16].try_into().expect("16 bytes");
    // SAFETY: `lane` is 16 readable bytes (the slice above is bounds-checked), the load's
    // width, and `vld1q_u8` has no alignment requirement.
    unsafe { std::arch::aarch64::vld1q_u8(lane.as_ptr()) }
}

/// [`blocks_scalar`] on NEON, 16 bytes per compare.
///
/// # Safety
///
/// The CPU has `neon` (callers check `is_aarch64_feature_detected!`); without it the
/// instructions fault.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
unsafe fn blocks_neon(bytes: &[u8], out: &mut Vec<[u64; 3]>) {
    const WEIGHTS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
    let weights = load_neon(&WEIGHTS, 0);
    for chunk in bytes.chunks_exact(64) {
        let (a, b, c, d) =
            (load_neon(chunk, 0), load_neon(chunk, 16), load_neon(chunk, 32), load_neon(chunk, 48));
        let (a, b, c, d) = (quarter_neon(a), quarter_neon(b), quarter_neon(c), quarter_neon(d));
        out.push([
            movemask_neon([a[0], b[0], c[0], d[0]], weights),
            movemask_neon([a[1], b[1], c[1], d[1]], weights),
            movemask_neon([a[2], b[2], c[2], d[2]], weights),
        ]);
    }
}

impl Classes {
    pub(super) fn new(bytes: &[u8]) -> Classes {
        Classes::with(bytes, blocks())
    }

    fn with(bytes: &[u8], blocks: Blocks) -> Classes {
        let whole = bytes.len() / 64 * 64;
        let mut out = Vec::with_capacity(bytes.len() / 64 + 1);
        blocks(&bytes[..whole], &mut out);
        let rest = &bytes[whole..];
        let mut tail = block_scalar(rest);
        // `rest` is shorter than 64 bytes, so the shift is in range.
        let past = !0u64 << rest.len();
        tail[IDENT] &= !past;
        tail[SPACE] &= !past;
        tail[NEWLINE] |= past;
        out.push(tail);
        Classes { blocks: out }
    }

    /// The first offset at or after `at` whose byte is not of `class` (the text's length at
    /// the latest).
    #[inline]
    fn run_end(&self, class: usize, at: usize) -> usize {
        let mut block = at / 64;
        let outside = !self.blocks[block][class] >> (at % 64);
        if outside != 0 {
            return at + outside.trailing_zeros() as usize;
        }
        loop {
            block += 1;
            let outside = !self.blocks[block][class];
            if outside != 0 {
                return block * 64 + outside.trailing_zeros() as usize;
            }
        }
    }

    /// The first offset at or after `at` whose byte is `\n`, or the text's length.
    #[inline]
    fn next_newline(&self, at: usize) -> usize {
        let mut block = at / 64;
        let found = self.blocks[block][NEWLINE] >> (at % 64);
        if found != 0 {
            return at + found.trailing_zeros() as usize;
        }
        loop {
            block += 1;
            let found = self.blocks[block][NEWLINE];
            if found != 0 {
                return block * 64 + found.trailing_zeros() as usize;
            }
        }
    }
}

/// The token `rustc_lexer` lexes at byte `at` of `bytes` (a whole text: no frontmatter, `at`
/// below its length), when the bytes decide it; `None` when `rustc_lexer` must.
#[inline]
pub(super) fn token(bytes: &[u8], at: usize, classes: &Classes) -> Option<(TokenKind, usize)> {
    let next = |at: usize| bytes.get(at).copied();
    let kind = match bytes[at] {
        b'a'..=b'z' | b'A'..=b'Z' | b'_' => {
            let end = classes.run_end(IDENT, at);
            // A quote or `#` after the run makes it a prefix (`r#`, `b"`, `br"`, `c"`, an
            // unknown one); a byte of 0x80 or more may continue it.
            return match next(end) {
                Some(b) if b >= 0x80 || matches!(b, b'#' | b'"' | b'\'') => None,
                _ => Some((TokenKind::Ident, end - at)),
            };
        }
        b'0'..=b'9' => {
            let end = classes.run_end(IDENT, at);
            if !bytes[at..end].iter().all(|&b| b.is_ascii_digit() || b == b'_') {
                // A base, an exponent or a suffix.
                return None;
            }
            let int = match next(end) {
                None => true,
                Some(b) if b >= 0x80 => false,
                // `0..2` and `1.foo` take no fraction; anything else after the `.` may.
                Some(b'.') => {
                    matches!(next(end + 1), Some(b'.' | b'a'..=b'z' | b'A'..=b'Z' | b'_'))
                }
                Some(_) => true,
            };
            let len = end - at;
            return int.then(|| {
                let kind = LiteralKind::Int { base: Base::Decimal, empty_int: false };
                (TokenKind::Literal { kind, suffix_start: len as u32 }, len)
            });
        }
        b'\t' | b'\n' | 0x0B | 0x0C | b'\r' | b' ' => {
            let end = classes.run_end(SPACE, at);
            // U+0085, U+200E, U+200F, U+2028 and U+2029 are whitespace too.
            return match next(end) {
                Some(b) if b >= 0x80 => None,
                _ => Some((TokenKind::Whitespace, end - at)),
            };
        }
        b'/' => match next(at + 1) {
            Some(b'/') => {
                let doc_style = match next(at + 2) {
                    Some(b'!') => Some(DocStyle::Inner),
                    Some(b'/') if next(at + 3) != Some(b'/') => Some(DocStyle::Outer),
                    _ => None,
                };
                let end = classes.next_newline(at + 2);
                return Some((TokenKind::LineComment { doc_style }, end - at));
            }
            Some(b'*') => return None,
            _ => TokenKind::Slash,
        },
        b'#' => match next(at + 1) {
            Some(b'"' | b'#') => return None,
            _ => TokenKind::Pound,
        },
        b';' => TokenKind::Semi,
        b',' => TokenKind::Comma,
        b'.' => TokenKind::Dot,
        b'(' => TokenKind::OpenParen,
        b')' => TokenKind::CloseParen,
        b'{' => TokenKind::OpenBrace,
        b'}' => TokenKind::CloseBrace,
        b'[' => TokenKind::OpenBracket,
        b']' => TokenKind::CloseBracket,
        b'@' => TokenKind::At,
        b'~' => TokenKind::Tilde,
        b'?' => TokenKind::Question,
        b':' => TokenKind::Colon,
        b'$' => TokenKind::Dollar,
        b'=' => TokenKind::Eq,
        b'!' => TokenKind::Bang,
        b'<' => TokenKind::Lt,
        b'>' => TokenKind::Gt,
        b'-' => TokenKind::Minus,
        b'&' => TokenKind::And,
        b'|' => TokenKind::Or,
        b'+' => TokenKind::Plus,
        b'*' => TokenKind::Star,
        b'^' => TokenKind::Caret,
        b'%' => TokenKind::Percent,
        _ => return None,
    };
    Some((kind, 1))
}

#[cfg(test)]
mod tests {
    use super::{Classes, paths, token};
    use crate::LexedStr;

    struct Rng(u64);

    impl Rng {
        fn below(&mut self, below: usize) -> usize {
            self.0 = self.0.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            ((self.0 >> 33) % below as u64) as usize
        }
    }

    /// Texts weighted to the bytes where the fast path and `rustc_lexer` could part: prefixes,
    /// quotes, `#`, `.` after digits, exponents, comments, Unicode whitespace and identifiers.
    fn random_text(rng: &mut Rng) -> String {
        const PIECES: &[&str] = &[
            "r", "b", "c", "br", "cr", "r#", "#", "##", "\"", "'", "'a", "_", "__", "0", "1", "9",
            "0x", "0b", "1e", "E", "1.", "..", ".", "1_000", "u8", "/", "//", "///", "////",
            "//!", "/*", "*/", "*", "\n", " ", "\t", "\r", "\x0B", "\x0C", "\u{85}", "\u{200E}",
            "\u{2028}", "é", "名", "🦀", "\u{200D}", "fn", "Self", "self", "let", "{", "}", "(",
            ")", ";", ":", "::", "<", ">", "=", "!", "?", "@", "$", "~", "^", "%", "&", "|", "+",
            "-", "\\", "x", "Z", "\0", "`",
        ];
        let mut text = String::new();
        for _ in 0..rng.below(200) {
            text.push_str(PIECES[rng.below(PIECES.len())]);
        }
        text
    }

    fn rust_files() -> Vec<String> {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"));
        let mut dirs = vec![root.join("src"), root.join("test_data")];
        let mut out = Vec::new();
        while let Some(dir) = dirs.pop() {
            for entry in std::fs::read_dir(&dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    dirs.push(path);
                } else if path.extension().is_some_and(|it| it == "rs") {
                    out.push(std::fs::read_to_string(&path).unwrap());
                }
            }
        }
        assert!(out.len() > 500, "the parser's sources and test files are found");
        out
    }

    /// Every vector path gives the scalar path's classes, for every length and alignment.
    #[test]
    fn every_path_classifies_as_the_scalar_one() {
        let mut rng = Rng(7);
        let mut texts: Vec<Vec<u8>> =
            (0..300).map(|len| (0..len).map(|_| rng.below(256) as u8).collect()).collect();
        texts.extend(rust_files().into_iter().take(50).map(String::into_bytes));
        for text in &texts {
            for skip in [0, 1, 31, 63] {
                let text = &text[skip.min(text.len())..];
                let scalar = Classes::with(text, paths()[0].1).blocks;
                for (name, blocks) in paths() {
                    let got = Classes::with(text, blocks).blocks;
                    assert_eq!(got, scalar, "{name} on {} bytes", text.len());
                }
            }
        }
    }

    /// Wherever the fast path gives a token, it is the token `rustc_lexer` lexes there: at
    /// every offset of the parser's sources, its test files and random texts.
    #[test]
    fn the_fast_path_lexes_as_rustc_lexer() {
        let mut rng = Rng(11);
        let mut texts = rust_files();
        texts.extend((0..3000).map(|_| random_text(&mut rng)));
        let mut decided = 0usize;
        for text in &texts {
            let classes = Classes::new(text.as_bytes());
            for at in (0..text.len()).filter(|&at| text.is_char_boundary(at)) {
                let Some((kind, len)) = token(text.as_bytes(), at, &classes) else { continue };
                decided += 1;
                let reference =
                    rustc_lexer::tokenize(&text[at..], rustc_lexer::FrontmatterAllowed::No)
                        .next()
                        .expect("a token where text is left");
                assert_eq!(
                    (kind, len),
                    (reference.kind, reference.len as usize),
                    "at {at} of {:?}",
                    &text[at..text.len().min(at + 40)],
                );
            }
        }
        assert!(decided > 100_000, "the fast path decides most tokens");
    }

    /// A text lexed with the fast path is the text lexed by `rustc_lexer` alone, in every
    /// edition: kinds, lengths, checkpoints, long tokens and errors.
    #[test]
    fn fast_and_rustc_lexer_lex_alike() {
        let mut rng = Rng(13);
        let mut texts = rust_files();
        texts.extend((0..2000).map(|_| random_text(&mut rng)));
        texts.push("#!/usr/bin/env cargo\n---\n[package]\n---\nfn main() {}\n".to_owned());
        texts.push("#!/bin/sh\nfn f() { r#\"x\"# ; #\"y\"# ## z }".to_owned());
        texts.push(format!("fn f() {{ \"{}\" }}", "s".repeat((1 << 22) + 3)));
        let errors = |it: &LexedStr<'_>| -> Vec<(usize, String)> {
            it.errors().map(|(i, e)| (i, e.to_owned())).collect()
        };
        for text in &texts {
            for edition in crate::Edition::iter() {
                let fast = LexedStr::lex::<true>(edition, text);
                let slow = LexedStr::lex::<false>(edition, text);
                assert!(
                    fast.tokens == slow.tokens
                        && fast.checkpoints == slow.checkpoints
                        && fast.long == slow.long
                        && errors(&fast) == errors(&slow),
                    "{edition:?}: {:?}",
                    &text[..text.len().min(80)],
                );
            }
        }
    }
}
