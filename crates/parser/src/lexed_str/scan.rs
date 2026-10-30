//! The lexer's branch-light front: byte classes 64 at a time, and the tokens they decide.
//!
//! [`Classes`] is four bitmaps over the text, made in one pass with the widest vector unit the
//! CPU has (AVX-512BW, AVX2 or NEON, chosen at run time; every path gives the bitmaps the scalar
//! one gives): which bytes are ASCII identifier bytes (`[A-Za-z0-9_]`), ASCII whitespace
//! (`\t\n\x0B\x0C\r `), `\n`, and `"` or `\`. A few integer ops per 64 bytes then replace the
//! whitespace bitmap by where a simple token may start: the first byte of an identifier or
//! whitespace run and every other byte. That costs 32 bytes per 64 of text, for the lexing only.
//!
//! [`token`] lexes the token at an offset from them when the bytes alone decide it the way
//! `rustc_lexer` does, and gives its final [`SyntaxKind`] (an identifier as `IDENT`, for the
//! caller's keyword lookup). An identifier, whitespace or punctuation token is one table lookup
//! for its kind, one `trailing_zeros` for its end (the next start bit) and one table lookup
//! for the byte after it. A line comment's end is the next newline bit, a string's the next
//! quote bit not escaped (its escapes then checked once), a lifetime's the next start bit, and
//! plain decimal integers are a byte or two of lookahead. That is nearly every token of Rust
//! source. A token that starts or ends where only `rustc_lexer`'s Unicode tables or its literal
//! grammar can say (an escape error, a char, a raw or prefixed string, a suffix, a block
//! comment, a byte of 0x80 or more next to the token) gives `None`,
//! and the caller asks `rustc_lexer` for that token. Both give the same kind and length wherever
//! `token` gives one, which the tests check at every offset of every test file; so this is
//! dispatch by token class, not a second lexer with its own rules.
//!
//! One of the crate's two unsafe places (docs/v2/plan/design/unsafe.md; the other is
//! `SyntaxKind::from(u16)`): the vector loads and the
//! `target_feature` calls are unsafe. The safe version is [`blocks_scalar`], a table lookup and
//! shift per byte, 64 per block; the vector paths do a block in a few compares. Every path is
//! checked against it on every test file (`every_path_classifies_as_the_scalar_one`).

use crate::{SyntaxKind, T};

const IDENT: usize = 0;
/// ASCII whitespace as classified; [`Classes::with`] replaces it by [`START`] in place.
const SPACE: usize = 1;
const START: usize = 1;
const NEWLINE: usize = 2;
const QUOTE: usize = 3;

/// The classes of 64 bytes: a bit per byte in each of [`IDENT`], [`START`] (first [`SPACE`]),
/// [`NEWLINE`] and [`QUOTE`].
type Block = [u64; 4];

/// The byte classes of a text, a [`Block`] per 64 bytes. Bits past the text's end are clear in
/// [`IDENT`] and set in the rest, so every run and search stops at the end. 32 bytes per 64 of
/// text, in a buffer the caller lends (the worker's scratch, so the warm loop allocates
/// nothing for them); a lookup is one load.
#[derive(Clone, Copy)]
pub(super) struct Classes<'b> {
    blocks: &'b [Block],
}

const _: () = assert!(size_of::<Block>() == 32, "32 bytes of classes per 64 of text");

/// Bit 0: an ASCII identifier byte; bit 1: ASCII whitespace; bit 2: `\n`; bit 3: `"` or `\`.
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
        if c == b'"' || c == b'\\' {
            table[b] |= 8;
        }
        b += 1;
    }
    table
};

/// The classes of 64 bytes or fewer, one byte at a time ([`SPACE`] where [`START`] goes).
fn block_scalar(bytes: &[u8]) -> Block {
    debug_assert!(bytes.len() <= 64);
    let mut out = [0u64; 4];
    for (i, &b) in bytes.iter().enumerate() {
        let class = CLASS[b as usize] as u64;
        out[IDENT] |= (class & 1) << i;
        out[SPACE] |= ((class >> 1) & 1) << i;
        out[NEWLINE] |= ((class >> 2) & 1) << i;
        out[QUOTE] |= ((class >> 3) & 1) << i;
    }
    out
}

/// The classes of every whole 64 bytes of `bytes`, appended to `out` a [`Block`]'s four words
/// at a time, with [`SPACE`] where [`START`] goes.
type Blocks = fn(&[u8], &mut Vec<u64>);

fn blocks_scalar(bytes: &[u8], out: &mut Vec<u64>) {
    for chunk in bytes.chunks_exact(64) {
        out.extend_from_slice(&block_scalar(chunk));
    }
}

/// The fastest [`Blocks`] this CPU has.
fn blocks() -> Blocks {
    #[cfg(target_arch = "x86_64")]
    {
        if is_x86_feature_detected!("avx512bw") {
            fn avx512(bytes: &[u8], out: &mut Vec<u64>) {
                // SAFETY: chosen only after `avx512bw` was detected.
                unsafe { blocks_avx512(bytes, out) }
            }
            return avx512;
        }
        if is_x86_feature_detected!("avx2") {
            fn avx2(bytes: &[u8], out: &mut Vec<u64>) {
                // SAFETY: chosen only after `avx2` was detected.
                unsafe { blocks_avx2(bytes, out) }
            }
            return avx2;
        }
    }
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("neon") {
        fn neon(bytes: &[u8], out: &mut Vec<u64>) {
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
            fn avx512(bytes: &[u8], out: &mut Vec<u64>) {
                // SAFETY: listed only after `avx512bw` was detected.
                unsafe { blocks_avx512(bytes, out) }
            }
            out.push(("avx512bw", avx512));
        }
        if is_x86_feature_detected!("avx2") {
            fn avx2(bytes: &[u8], out: &mut Vec<u64>) {
                // SAFETY: listed only after `avx2` was detected.
                unsafe { blocks_avx2(bytes, out) }
            }
            out.push(("avx2", avx2));
        }
    }
    #[cfg(target_arch = "aarch64")]
    if std::arch::is_aarch64_feature_detected!("neon") {
        fn neon(bytes: &[u8], out: &mut Vec<u64>) {
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
unsafe fn blocks_avx512(bytes: &[u8], out: &mut Vec<u64>) {
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
    let quote = _mm512_set1_epi8(b'"' as i8);
    let backslash = _mm512_set1_epi8(b'\\' as i8);
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
        let quotes = _mm512_cmpeq_epi8_mask(v, quote) | _mm512_cmpeq_epi8_mask(v, backslash);
        out.extend_from_slice(&[ident, blank, _mm512_cmpeq_epi8_mask(v, newline), quotes]);
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

/// The four classes of 32 bytes.
#[cfg(target_arch = "x86_64")]
#[target_feature(enable = "avx2")]
#[inline]
fn half_avx2(v: std::arch::x86_64::__m256i) -> [u64; 4] {
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
    let quotes = _mm256_or_si256(
        _mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'"' as i8)),
        _mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'\\' as i8)),
    );
    [
        _mm256_movemask_epi8(ident) as u32 as u64,
        _mm256_movemask_epi8(blank) as u32 as u64,
        _mm256_movemask_epi8(newline) as u32 as u64,
        _mm256_movemask_epi8(quotes) as u32 as u64,
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
unsafe fn blocks_avx2(bytes: &[u8], out: &mut Vec<u64>) {
    use std::arch::x86_64::*;
    for chunk in bytes.chunks_exact(64) {
        let (low, high) = chunk.split_at(32);
        // SAFETY: `low` is 32 readable bytes (`chunks_exact(64)` split in half), the load's
        // width, and `loadu` has no alignment requirement.
        let lo = unsafe { _mm256_loadu_si256(low.as_ptr().cast()) };
        // SAFETY: as for `low`: `high` is the other 32 bytes of the chunk.
        let hi = unsafe { _mm256_loadu_si256(high.as_ptr().cast()) };
        let (lo, hi) = (half_avx2(lo), half_avx2(hi));
        out.extend_from_slice(&[
            lo[0] | hi[0] << 32,
            lo[1] | hi[1] << 32,
            lo[2] | hi[2] << 32,
            lo[3] | hi[3] << 32,
        ]);
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

/// The four classes of 16 bytes, as 0xFF or 0 per byte.
#[cfg(target_arch = "aarch64")]
#[target_feature(enable = "neon")]
#[inline]
fn quarter_neon(v: std::arch::aarch64::uint8x16_t) -> [std::arch::aarch64::uint8x16_t; 4] {
    use std::arch::aarch64::*;
    let lower = vorrq_u8(v, vdupq_n_u8(0x20));
    let alpha = vcleq_u8(vsubq_u8(lower, vdupq_n_u8(b'a')), vdupq_n_u8(25));
    let digit = vcleq_u8(vsubq_u8(v, vdupq_n_u8(b'0')), vdupq_n_u8(9));
    let ident = vorrq_u8(vorrq_u8(alpha, digit), vceqq_u8(v, vdupq_n_u8(b'_')));
    let control = vcleq_u8(vsubq_u8(v, vdupq_n_u8(b'\t')), vdupq_n_u8(4));
    let blank = vorrq_u8(control, vceqq_u8(v, vdupq_n_u8(b' ')));
    let quotes = vorrq_u8(vceqq_u8(v, vdupq_n_u8(b'"')), vceqq_u8(v, vdupq_n_u8(b'\\')));
    [ident, blank, vceqq_u8(v, vdupq_n_u8(b'\n')), quotes]
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
unsafe fn blocks_neon(bytes: &[u8], out: &mut Vec<u64>) {
    const WEIGHTS: [u8; 16] = [1, 2, 4, 8, 16, 32, 64, 128, 1, 2, 4, 8, 16, 32, 64, 128];
    let weights = load_neon(&WEIGHTS, 0);
    for chunk in bytes.chunks_exact(64) {
        let (a, b, c, d) =
            (load_neon(chunk, 0), load_neon(chunk, 16), load_neon(chunk, 32), load_neon(chunk, 48));
        let (a, b, c, d) = (quarter_neon(a), quarter_neon(b), quarter_neon(c), quarter_neon(d));
        out.extend_from_slice(&[
            movemask_neon([a[0], b[0], c[0], d[0]], weights),
            movemask_neon([a[1], b[1], c[1], d[1]], weights),
            movemask_neon([a[2], b[2], c[2], d[2]], weights),
            movemask_neon([a[3], b[3], c[3], d[3]], weights),
        ]);
    }
}

impl<'b> Classes<'b> {
    /// The classes of `bytes`, written over `words` (emptied first).
    pub(super) fn new(bytes: &[u8], words: &'b mut Vec<u64>) -> Classes<'b> {
        Classes::with(bytes, blocks(), words)
    }

    fn with(bytes: &[u8], blocks: Blocks, words: &'b mut Vec<u64>) -> Classes<'b> {
        let whole = bytes.len() / 64 * 64;
        words.clear();
        words.reserve(4 * (bytes.len() / 64 + 1));
        blocks(&bytes[..whole], words);
        let rest = &bytes[whole..];
        let mut tail = block_scalar(rest);
        // `rest` is shorter than 64 bytes, so the shift is in range.
        let past = !0u64 << rest.len();
        tail[IDENT] &= !past;
        tail[SPACE] &= !past;
        tail[NEWLINE] |= past;
        tail[QUOTE] |= past;
        words.extend_from_slice(&tail);
        // A run's first byte and every byte of neither run starts a token; so do the bits past
        // the end, as both runs are clear there. `START` takes `SPACE`'s place.
        let (mut carry_ident, mut carry_space) = (0u64, 0u64);
        for block in words.as_chunks_mut::<4>().0 {
            let (ident, space) = (block[IDENT], block[SPACE]);
            let runs = (ident & !(ident << 1 | carry_ident)) | (space & !(space << 1 | carry_space));
            block[START] = runs | !(ident | space);
            (carry_ident, carry_space) = (ident >> 63, space >> 63);
        }
        Classes { blocks: words.as_chunks::<4>().0 }
    }

    /// The first offset after `at` where a token may start (the text's length at the latest):
    /// the end of the identifier or whitespace run at `at`, or `at + 1` after any other byte.
    #[inline]
    fn next_start(&self, at: usize) -> usize {
        self.next_bit(START, at + 1)
    }

    /// The first offset at or after `at` whose byte is `\n`, or the text's length.
    #[inline]
    fn next_newline(&self, at: usize) -> usize {
        self.next_bit(NEWLINE, at)
    }

    /// The first offset at or after `at` whose byte is `"` or `\`, or the text's length.
    #[inline]
    fn next_quote(&self, at: usize) -> usize {
        self.next_bit(QUOTE, at)
    }

    /// The first set bit of `class` at or after `at`, at most the text's length (`at` is at
    /// most that, and `class` is set past it).
    #[inline]
    fn next_bit(&self, class: usize, at: usize) -> usize {
        let mut block = at / 64;
        let found = self.blocks[block][class] >> (at % 64);
        if found != 0 {
            return at + found.trailing_zeros() as usize;
        }
        loop {
            block += 1;
            let found = self.blocks[block][class];
            if found != 0 {
                return block * 64 + found.trailing_zeros() as usize;
            }
        }
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
}

/// A token's kind on the fast path by its first byte: `IDENT` for an identifier run (the caller
/// looks up keywords), `WHITESPACE`, a punctuation kind, or for the bytes [`special`] decides
/// `INT_NUMBER` (digits), `SLASH`, `POUND` and `EOF` (quotes and the bytes `rustc_lexer` lexes).
const FIRST: [SyntaxKind; 256] = {
    let mut table = [SyntaxKind::EOF; 256];
    let mut b = 0;
    while b < 256 {
        let c = b as u8;
        if c.is_ascii_alphabetic() || c == b'_' {
            table[b] = SyntaxKind::IDENT;
        } else if c.is_ascii_digit() {
            table[b] = SyntaxKind::INT_NUMBER;
        } else if matches!(c, b'\t' | b'\n' | 0x0B | 0x0C | b'\r' | b' ') {
            table[b] = SyntaxKind::WHITESPACE;
        }
        b += 1;
    }
    let punct: [(u8, SyntaxKind); 26] = [
        (b';', T![;]),
        (b',', T![,]),
        (b'.', T![.]),
        (b'(', T!['(']),
        (b')', T![')']),
        (b'{', T!['{']),
        (b'}', T!['}']),
        (b'[', T!['[']),
        (b']', T![']']),
        (b'@', T![@]),
        (b'~', T![~]),
        (b'?', T![?]),
        (b':', T![:]),
        (b'$', T![$]),
        (b'=', T![=]),
        (b'!', T![!]),
        (b'<', T![<]),
        (b'>', T![>]),
        (b'-', T![-]),
        (b'&', T![&]),
        (b'|', T![|]),
        (b'+', T![+]),
        (b'*', T![*]),
        (b'^', T![^]),
        (b'%', T![%]),
        (b'/', T![/]),
    ];
    let mut i = 0;
    while i < punct.len() {
        table[punct[i].0 as usize] = punct[i].1;
        i += 1;
    }
    table[b'#' as usize] = T![#];
    table
};

/// [`FLAGS`] of a first byte: an identifier run, a whitespace run, or one [`special`] lexes.
const IDENT_RUN: u8 = 1;
const SPACE_RUN: u8 = 2;
const SPECIAL: u8 = 4;

const FLAGS: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut b = 0;
    while b < 256 {
        table[b] = match FIRST[b] {
            SyntaxKind::IDENT => IDENT_RUN,
            SyntaxKind::WHITESPACE => SPACE_RUN,
            SyntaxKind::INT_NUMBER | SyntaxKind::SLASH | SyntaxKind::POUND | SyntaxKind::EOF => {
                SPECIAL
            }
            _ => 0,
        };
        b += 1;
    }
    table
};

/// The runs a byte right after them leaves to `rustc_lexer`: a quote or `#` after an
/// identifier makes it a prefix (`r#`, `b"`, `br"`, `c"`, an unknown one), and a byte of 0x80
/// or more may continue either (an identifier, or U+0085, U+200E, U+200F, U+2028 or U+2029).
const STOPS: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut b = 0;
    while b < 256 {
        if b >= 0x80 {
            table[b] = IDENT_RUN | SPACE_RUN;
        } else if matches!(b as u8, b'#' | b'"' | b'\'') {
            table[b] = IDENT_RUN;
        }
        b += 1;
    }
    table
};

/// A position in a text and the [`START`] bits after it, in a register: the next token start is
/// one `trailing_zeros`, and moving to it clears the lowest bit. So the chain from one token to
/// the next is one dependent ALU op, not a load at the last token's end (simdjson's stage 2).
pub(super) struct Cursor<'c> {
    classes: Classes<'c>,
    block: usize,
    /// The block's [`START`] bits after the position; zero when the next is in a later block.
    bits: u64,
}

impl<'c> Cursor<'c> {
    pub(super) fn new(classes: &Classes<'c>, at: usize) -> Cursor<'c> {
        let mut cursor = Cursor { classes: *classes, block: 0, bits: 0 };
        cursor.seek(at);
        cursor
    }

    /// Moves to `at` (at most the text's length).
    #[inline]
    pub(super) fn seek(&mut self, at: usize) {
        self.block = at / 64;
        // Two shifts, as `at % 64 + 1` may be 64.
        self.bits = self.classes.blocks[self.block][START] & ((!0u64 << (at % 64)) << 1);
    }

    /// The next token start after the position: [`Classes::next_start`] of it. The position is
    /// below the text's length, so a start bit is left (the text's end is one).
    #[inline]
    fn peek(&mut self) -> usize {
        while self.bits == 0 {
            self.block += 1;
            self.bits = self.classes.blocks[self.block][START];
        }
        self.block * 64 + self.bits.trailing_zeros() as usize
    }

    /// Moves to [`Cursor::peek`]'s start.
    #[inline]
    fn step(&mut self) {
        self.bits &= self.bits - 1;
    }
}

/// The token `rustc_lexer` lexes at byte `at` of `text` (a whole text: no frontmatter, `at`
/// below its length, the cursor at `at`), as the lexer converts it, when the bytes decide it;
/// `None` when `rustc_lexer` must. An identifier is `IDENT`, for the caller's keyword lookup.
/// After a token the cursor is at its end; after `None` the caller moves it.
///
/// An identifier, whitespace or punctuation token, most of a text, takes three table loads off
/// the dependency chain, one `trailing_zeros` and two well-predicted branches; the rest is
/// [`special`].
#[inline]
pub(super) fn token(text: &str, at: usize, cursor: &mut Cursor<'_>) -> Option<(SyntaxKind, usize)> {
    let bytes = text.as_bytes();
    let first = bytes[at] as usize;
    let flags = FLAGS[first];
    if flags & SPECIAL != 0 {
        let (kind, len) = special(text, at, &cursor.classes)?;
        cursor.seek(at + len);
        return Some((kind, len));
    }
    let end = cursor.peek();
    let after = bytes.get(end).copied().unwrap_or(0);
    if STOPS[after as usize] & flags != 0 {
        return None;
    }
    cursor.step();
    Some((FIRST[first], end - at))
}

/// [`token`] at a digit, `/`, `#`, `"`, `'` or a byte only `rustc_lexer` lexes.
fn special(text: &str, at: usize, classes: &Classes<'_>) -> Option<(SyntaxKind, usize)> {
    let bytes = text.as_bytes();
    let next = |at: usize| bytes.get(at).copied();
    match bytes[at] {
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
            int.then_some((SyntaxKind::INT_NUMBER, end - at))
        }
        b'/' => match next(at + 1) {
            Some(b'/') => {
                let kind = match next(at + 2) {
                    Some(b'!') => SyntaxKind::INNER_DOC_COMMENT,
                    Some(b'/') if next(at + 3) != Some(b'/') => SyntaxKind::OUTER_DOC_COMMENT,
                    _ => SyntaxKind::COMMENT,
                };
                Some((kind, classes.next_newline(at + 2) - at))
            }
            Some(b'*') => None,
            _ => Some((T![/], 1)),
        },
        b'#' => match next(at + 1) {
            Some(b'"' | b'#') => None,
            _ => Some((T![#], 1)),
        },
        // A string ends at the first quote not escaped: from quote bit to quote bit, a `\`
        // taking the byte after it as `rustc_lexer` does (a byte, so a UTF-8 lead at most; the
        // rest of the character is no quote bit). With no escape it has no error (a bare `\r`
        // is the converter's no-message error); with one, its escapes are checked as the
        // converter checks them, and any error is left to it. A suffix may follow it.
        b'"' => {
            let (mut from, mut escaped) = (at + 1, false);
            let close = loop {
                let found = classes.next_quote(from);
                match next(found) {
                    Some(b'"') => break found,
                    Some(_) if found + 2 <= text.len() => (from, escaped) = (found + 2, true),
                    _ => return None,
                }
            };
            if let Some(b) = next(close + 1)
                && (b >= 0x80 || b.is_ascii_alphabetic() || b == b'_')
            {
                return None;
            }
            if escaped {
                let mut valid = true;
                rustc_literal_escaper::unescape_str(&text[at + 1..close], |_, it| {
                    valid &= it.is_ok();
                });
                if !valid {
                    return None;
                }
            }
            Some((SyntaxKind::STRING, close + 1 - at))
        }
        // `'` and an ASCII identifier run: a lifetime unless `'` (a char), `#` (a raw lifetime or
        // a prefix) or a byte of 0x80 or more follows the run.
        b'\'' => {
            let c = next(at + 1)?;
            if !(c.is_ascii_alphabetic() || c == b'_') {
                return None;
            }
            let end = classes.next_start(at + 1);
            match next(end) {
                Some(b'\'' | b'#') => None,
                Some(b) if b >= 0x80 => None,
                _ => Some((SyntaxKind::LIFETIME_IDENT, end - at)),
            }
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use blaze_ids::rng::SplitMix64;

    use super::super::{Converter, ident_kind};
    use super::{Classes, Cursor, paths, token};
    use crate::{LexedStr, SyntaxKind};

    /// Texts weighted to the bytes where the fast path and `rustc_lexer` could part: prefixes,
    /// quotes, `#`, `.` after digits, exponents, comments, Unicode whitespace and identifiers.
    fn random_text(rng: &mut SplitMix64) -> String {
        const PIECES: &[&str] = &[
            "r", "b", "c", "br", "cr", "r#", "#", "##", "\"", "'", "'a", "_", "__", "0", "1", "9",
            "0x", "0b", "1e", "E", "1.", "..", ".", "1_000", "u8", "/", "//", "///", "////",
            "//!", "/*", "*/", "*", "\n", " ", "\t", "\r", "\x0B", "\x0C", "\u{85}", "\u{200E}",
            "\u{2028}", "é", "名", "🦀", "\u{200D}", "fn", "Self", "self", "let", "{", "}", "(",
            ")", ";", ":", "::", "<", ">", "=", "!", "?", "@", "$", "~", "^", "%", "&", "|", "+",
            "-", "\\", "x", "Z", "\0", "`", "\\n", "\\\"", "\\\\", "\\u{41}", "\\u{D800}", "\\x7f",
            "\\x80", "\\\n", "\\q", "\"\\", "\"\"",
        ];
        let mut text = String::new();
        for _ in 0..rng.index(200) {
            text.push_str(PIECES[rng.index(PIECES.len())]);
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
        let mut rng = SplitMix64::new(7);
        let mut texts: Vec<Vec<u8>> =
            (0..300).map(|len| (0..len).map(|_| rng.index(256) as u8).collect()).collect();
        texts.extend(rust_files().into_iter().take(50).map(String::into_bytes));
        for text in &texts {
            for skip in [0, 1, 31, 63] {
                let text = &text[skip.min(text.len())..];
                let mut words = Vec::new();
                let scalar = Classes::with(text, paths()[0].1, &mut words).blocks.to_vec();
                for (name, blocks) in paths() {
                    let mut words = vec![!0; 9];
                    let got = Classes::with(text, blocks, &mut words).blocks.to_vec();
                    assert_eq!(got, scalar, "{name} on {} bytes", text.len());
                }
            }
        }
    }

    /// The next start bit after any offset is the end of the identifier or whitespace run there,
    /// or the next byte, and the next quote bit the next `"` or `\`, on every path's classes.
    #[test]
    fn the_next_start_is_where_the_run_ends() {
        let mut rng = SplitMix64::new(17);
        let mut texts: Vec<Vec<u8>> =
            (0..300).map(|len| (0..len).map(|_| rng.index(256) as u8).collect()).collect();
        texts.extend((0..300).map(|_| random_text(&mut rng).into_bytes()));
        texts.extend(rust_files().into_iter().take(50).map(String::into_bytes));
        for text in &texts {
            for (name, blocks) in paths() {
                let mut words = Vec::new();
                let classes = Classes::with(text, blocks, &mut words);
                let ident = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
                let space = |b: u8| matches!(b, b'\t' | b'\n' | 0x0B | 0x0C | b'\r' | b' ');
                for at in 0..text.len() {
                    let class: Option<&dyn Fn(u8) -> bool> = match text[at] {
                        b if ident(b) => Some(&ident),
                        b if space(b) => Some(&space),
                        _ => None,
                    };
                    let expected = match class {
                        Some(class) => {
                            at + text[at..].iter().take_while(|&&b| class(b)).count()
                        }
                        None => at + 1,
                    };
                    let quote = at
                        + text[at..].iter().take_while(|&&b| b != b'"' && b != b'\\').count();
                    let len = text.len();
                    assert_eq!(classes.next_start(at), expected, "{name} at {at} of {len}");
                    assert_eq!(classes.next_quote(at), quote, "{name} at {at} of {len}");
                    assert_eq!(Cursor::new(&classes, at).peek(), expected, "{name} at {at}");
                }
                // Stepping from start to start visits each start once, in order.
                if !text.is_empty() {
                    let mut cursor = Cursor::new(&classes, 0);
                    let mut at = 0;
                    while at < text.len() {
                        let next = cursor.peek();
                        assert_eq!(next, classes.next_start(at), "{name} stepping at {at}");
                        cursor.step();
                        at = next;
                    }
                    assert_eq!(at, text.len());
                }
            }
        }
    }

    /// Wherever the fast path gives a token, it is the token `rustc_lexer` lexes there as the
    /// lexer converts it, with no error: at every offset of the parser's sources, its test
    /// files and random texts.
    #[test]
    fn the_fast_path_lexes_as_rustc_lexer() {
        let mut rng = SplitMix64::new(11);
        let mut texts = rust_files();
        texts.extend((0..3000).map(|_| random_text(&mut rng)));
        let mut decided = 0usize;
        let edition = crate::Edition::CURRENT;
        for text in &texts {
            let mut words = Vec::new();
            let classes = Classes::new(text.as_bytes(), &mut words);
            for at in (0..text.len()).filter(|&at| text.is_char_boundary(at)) {
                let mut cursor = Cursor::new(&classes, at);
                let Some((kind, len)) = token(text, at, &mut cursor) else { continue };
                decided += 1;
                if at + len < text.len() {
                    let (mut after, mut fresh) = (cursor, Cursor::new(&classes, at + len));
                    assert_eq!(after.peek(), fresh.peek(), "the cursor is at the token's end");
                }
                let rest = &text[at..];
                let reference = rustc_lexer::tokenize(rest, rustc_lexer::FrontmatterAllowed::No)
                    .next()
                    .expect("a token where text is left");
                let mut conv = Converter::new(edition, rest);
                conv.extend_token(&reference.kind, &rest[..reference.len as usize]);
                let kind = match kind {
                    SyntaxKind::IDENT => ident_kind(rest, 0, len, edition),
                    kind => kind,
                };
                assert!(
                    (kind, len) == (conv.res.raw_kind(0), conv.offset)
                        && conv.res.tokens.len() == 1
                        && conv.res.error.is_empty(),
                    "at {at} of {:?}: {:?}",
                    &text[at..text.len().min(at + 40)],
                    (kind, len, reference.kind),
                );
            }
        }
        assert!(decided > 100_000, "the fast path decides most tokens");
    }

    /// A text lexed with the fast path is the text lexed by `rustc_lexer` alone, in every
    /// edition: kinds, lengths, checkpoints, long tokens and errors.
    #[test]
    fn fast_and_rustc_lexer_lex_alike() {
        let mut rng = SplitMix64::new(13);
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
