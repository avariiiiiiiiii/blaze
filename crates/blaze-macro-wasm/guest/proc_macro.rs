//! The `proc_macro` crate of a proc macro blaze compiles to WebAssembly, as dtolnay's watt
//! runs one: blaze puts this text in place of the standard library's `library/proc_macro`
//! root when it compiles a proc-macro crate for `wasm32-wasip1`
//! (`crates/blaze-macro-wasm/src/lib.rs` says how the module is run). `proc-macro2`, `syn`
//! and `quote` compile against it unchanged.
//!
//! Token trees live in the module's memory: a stream is a vector of them, an identifier or a
//! literal holds its text. A span is a word of the host's (its range and the index of its
//! anchor and context among the expansion's, packed as a token tree packs one), and what can
//! only be known of it there (joining two, its source text, its file) is asked of the host by
//! an import. The input of a macro arrives, and its output leaves, as a token tree's columns
//! ([`wire`]). Displaying a
//! stream follows the proc-macro server blaze expands native macros with
//! (`vendor/proc-macro-srv/crates/proc-macro-srv/src/token_stream.rs`), so a macro that
//! compares `to_string()`s sees the same text either way.

#![allow(clippy::all, unused, missing_docs)]

use std::{
    cell::RefCell,
    error,
    ffi::CStr,
    fmt,
    iter::FromIterator,
    ops::{Bound, Range, RangeBounds},
    path::PathBuf,
    rc::Rc,
    str::FromStr,
};

mod sys {
    #[link(wasm_import_module = "blaze_proc_macro")]
    unsafe extern "C" {
        pub fn def_site() -> u64;
        pub fn call_site() -> u64;
        pub fn mixed_site() -> u64;
        pub fn span_join(a: u64, b: u64) -> u64;
        pub fn span_resolved_at(a: u64, b: u64) -> u64;
        pub fn span_located_at(a: u64, b: u64) -> u64;
        pub fn span_parent(a: u64) -> u64;
        pub fn span_source(a: u64) -> u64;
        pub fn span_start(a: u64) -> u64;
        pub fn span_end(a: u64) -> u64;
        pub fn span_subspan(a: u64, start: u64, end: u64) -> u64;
        pub fn span_byte_range(a: u64, out: *mut u32);
        pub fn span_line(a: u64) -> u32;
        pub fn span_column(a: u64) -> u32;
        /// The word of the span `start..start + len` of the expansion's part `part`, for one
        /// that doesn't pack.
        pub fn span_pack(start: u32, len: u32, part: u32) -> u64;
        /// Writes `[start, len, part]` of a span that doesn't pack to `out`.
        pub fn span_unpack(a: u64, out: *mut u32);
        /// The length of the text the host put in its buffer for [`take`], or -1 for none.
        pub fn span_debug(a: u64) -> i32;
        pub fn span_file(a: u64) -> i32;
        pub fn span_local_file(a: u64) -> i32;
        pub fn span_source_text(a: u64) -> i32;
        /// A stream lexed from the text, as a tree's columns: their length in the buffer, or
        /// `-1 - len` with the error's text of `len` bytes there.
        pub fn ts_from_str(ptr: *const u8, len: usize) -> i32;
        pub fn literal_from_str(ptr: *const u8, len: usize) -> i32;
        /// Copies the host's buffer to `to`.
        pub fn take(to: *mut u8);
        pub fn track_path(ptr: *const u8, len: usize);
        pub fn track_env_var(ptr: *const u8, len: usize);
        pub fn diagnostic(level: u32, ptr: *const u8, len: usize, span: u64);
        /// The macro's output, encoded.
        pub fn output(ptr: *const u8, len: usize);
        /// What a panic said, before the module traps.
        pub fn panicked(ptr: *const u8, len: usize);
        /// That the expansion left `bytes` of what it allocated live when it returned.
        pub fn retained(bytes: usize);
    }
}

/// The module's allocator: the standard library's, counting the bytes live, so that the host
/// learns whether an expansion left anything behind for a later one (`__blaze::expand1`).
struct Counted;

static LIVE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn live() -> usize {
    LIVE.load(std::sync::atomic::Ordering::Relaxed)
}

// SAFETY: every method passes its arguments to `System` unchanged and returns what it returns; the
// counter is only a statistic.
unsafe impl std::alloc::GlobalAlloc for Counted {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc`'s contract for this call, which is `System`'s.
        let at = unsafe { std::alloc::System.alloc(layout) };
        if !at.is_null() {
            LIVE.fetch_add(layout.size(), std::sync::atomic::Ordering::Relaxed);
        }
        at
    }

    unsafe fn alloc_zeroed(&self, layout: std::alloc::Layout) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc`'s contract for this call, which is `System`'s.
        let at = unsafe { std::alloc::System.alloc_zeroed(layout) };
        if !at.is_null() {
            LIVE.fetch_add(layout.size(), std::sync::atomic::Ordering::Relaxed);
        }
        at
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: std::alloc::Layout) {
        // SAFETY: the caller upholds `GlobalAlloc`'s contract for this call, which is `System`'s.
        unsafe { std::alloc::System.dealloc(ptr, layout) };
        LIVE.fetch_sub(layout.size(), std::sync::atomic::Ordering::Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: std::alloc::Layout, size: usize) -> *mut u8 {
        // SAFETY: the caller upholds `GlobalAlloc`'s contract for this call, which is `System`'s.
        let at = unsafe { std::alloc::System.realloc(ptr, layout, size) };
        if !at.is_null() {
            LIVE.fetch_add(size, std::sync::atomic::Ordering::Relaxed);
            LIVE.fetch_sub(layout.size(), std::sync::atomic::Ordering::Relaxed);
        }
        at
    }
}

#[global_allocator]
static ALLOCATOR: Counted = Counted;

fn taken(len: i32) -> Option<Vec<u8>> {
    let len = usize::try_from(len).ok()?;
    let mut bytes = vec![0u8; len];
    // SAFETY: `bytes` has `len` bytes, the length the host answered for what it holds, which
    // `sys::take` writes whole; a shorter buffer would be written past.
    unsafe { sys::take(bytes.as_mut_ptr()) };
    Some(bytes)
}

fn taken_string(len: i32) -> Option<String> {
    taken(len).map(|it| String::from_utf8(it).unwrap_or_default())
}

pub fn is_available() -> bool {
    true
}

#[derive(Clone)]
pub struct TokenStream(Rc<Vec<TokenTree>>);

#[derive(Debug)]
pub struct LexError(String);

impl fmt::Display for LexError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

impl error::Error for LexError {}

#[derive(Debug)]
pub struct ExpandError;

impl fmt::Display for ExpandError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("macro expansion failed")
    }
}

impl error::Error for ExpandError {}

impl TokenStream {
    pub fn new() -> TokenStream {
        TokenStream(Rc::new(Vec::new()))
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// As rust-analyzer's server does: an expression is not expanded early.
    pub fn expand_expr(&self) -> Result<TokenStream, ExpandError> {
        Ok(self.clone())
    }

    fn trees(&mut self) -> &mut Vec<TokenTree> {
        Rc::make_mut(&mut self.0)
    }
}

impl FromStr for TokenStream {
    type Err = LexError;

    fn from_str(src: &str) -> Result<TokenStream, LexError> {
        // SAFETY: `src` is borrowed for the call, so its `src.len()` bytes at `src.as_ptr()` stay
        // live and unchanged; the host only reads them, during the call, and copies what it keeps.
        let len = unsafe { sys::ts_from_str(src.as_ptr(), src.len()) };
        match taken(len) {
            Some(bytes) => Ok(wire::Reader::new(&bytes).contents()),
            None => Err(LexError(taken_string(-1 - len).unwrap_or_default())),
        }
    }
}

impl fmt::Display for TokenStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut emit_whitespace = false;
        for tt in self.0.iter() {
            display_tree(tt, &mut emit_whitespace, f)?;
        }
        Ok(())
    }
}

fn display_tree(
    tt: &TokenTree,
    emit_whitespace: &mut bool,
    f: &mut fmt::Formatter<'_>,
) -> fmt::Result {
    if std::mem::take(emit_whitespace) {
        f.write_str(" ")?;
    }
    match tt {
        TokenTree::Group(group) => {
            let (open, close) = group.delimiter.text();
            f.write_str(open)?;
            fmt::Display::fmt(&group.stream, f)?;
            f.write_str(close)?;
        }
        TokenTree::Punct(punct) => {
            *emit_whitespace = !punct.joint;
            write!(f, "{}", punct.ch)?;
        }
        TokenTree::Ident(ident) => {
            fmt::Display::fmt(ident, f)?;
            *emit_whitespace = true;
        }
        TokenTree::Literal(literal) => {
            fmt::Display::fmt(literal, f)?;
            *emit_whitespace = !matches!(
                literal.kind,
                LitKind::Str
                    | LitKind::StrRaw(_)
                    | LitKind::ByteStr
                    | LitKind::ByteStrRaw(_)
                    | LitKind::CStr
                    | LitKind::CStrRaw(_)
            );
        }
    }
    Ok(())
}

impl fmt::Debug for TokenStream {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("TokenStream ")?;
        f.debug_list().entries(self.0.iter()).finish()
    }
}

impl Default for TokenStream {
    fn default() -> Self {
        TokenStream::new()
    }
}

impl From<TokenTree> for TokenStream {
    fn from(tree: TokenTree) -> TokenStream {
        TokenStream(Rc::new(vec![tree]))
    }
}

impl FromIterator<TokenTree> for TokenStream {
    fn from_iter<I: IntoIterator<Item = TokenTree>>(trees: I) -> Self {
        TokenStream(Rc::new(trees.into_iter().collect()))
    }
}

impl FromIterator<TokenStream> for TokenStream {
    fn from_iter<I: IntoIterator<Item = TokenStream>>(streams: I) -> Self {
        let mut all = TokenStream::new();
        all.extend(streams);
        all
    }
}

impl Extend<TokenTree> for TokenStream {
    fn extend<I: IntoIterator<Item = TokenTree>>(&mut self, trees: I) {
        self.trees().extend(trees);
    }
}

impl Extend<TokenStream> for TokenStream {
    fn extend<I: IntoIterator<Item = TokenStream>>(&mut self, streams: I) {
        for stream in streams {
            if self.0.is_empty() {
                *self = stream;
            } else {
                self.trees().extend(stream.0.iter().cloned());
            }
        }
    }
}

macro_rules! extend_items {
    ($($item:ident)*) => {
        $(
            impl Extend<$item> for TokenStream {
                fn extend<T: IntoIterator<Item = $item>>(&mut self, iter: T) {
                    self.extend(iter.into_iter().map(TokenTree::$item));
                }
            }
        )*
    };
}

extend_items!(Group Literal Punct Ident);

pub mod token_stream {
    use super::{TokenStream, TokenTree};

    #[derive(Clone)]
    pub struct IntoIter {
        stream: TokenStream,
        at: usize,
    }

    impl Iterator for IntoIter {
        type Item = TokenTree;

        fn next(&mut self) -> Option<TokenTree> {
            let tree = self.stream.0.get(self.at)?.clone();
            self.at += 1;
            Some(tree)
        }

        fn size_hint(&self) -> (usize, Option<usize>) {
            let left = self.stream.0.len() - self.at;
            (left, Some(left))
        }
    }

    impl IntoIterator for TokenStream {
        type Item = TokenTree;
        type IntoIter = IntoIter;

        fn into_iter(self) -> IntoIter {
            IntoIter { stream: self, at: 0 }
        }
    }
}

/// A word of the host's: `start | len_part << 32`, `len_part` being `len | part << 16` with
/// `part` an index of the expansion's span parts, or `u32::MAX` with `start` an index of the
/// host's spans too wide to pack. Equal spans have equal words.
#[derive(Copy, Clone, PartialEq, Eq, Hash)]
pub struct Span(u64);

impl Span {
    pub fn def_site() -> Span {
        // SAFETY: the `sys` call takes and gives integers only, so it reads and writes no memory of
        // this crate's, and nothing the caller holds can break.
        Span(unsafe { sys::def_site() })
    }

    pub fn call_site() -> Span {
        // SAFETY: the `sys` call takes and gives integers only, so it reads and writes no memory of
        // this crate's, and nothing the caller holds can break.
        Span(unsafe { sys::call_site() })
    }

    pub fn mixed_site() -> Span {
        // SAFETY: the `sys` call takes and gives integers only, so it reads and writes no memory of
        // this crate's, and nothing the caller holds can break.
        Span(unsafe { sys::mixed_site() })
    }

    /// The word of no span.
    const NONE: u64 = u64::MAX;

    fn some(word: u64) -> Option<Span> {
        (word != Span::NONE).then_some(Span(word))
    }

    pub fn parent(&self) -> Option<Span> {
        // SAFETY: the `sys` call takes and gives integers only, so it reads and writes no memory of
        // this crate's, and nothing the caller holds can break.
        Span::some(unsafe { sys::span_parent(self.0) })
    }

    pub fn source(&self) -> Span {
        // SAFETY: the `sys` call takes and gives integers only, so it reads and writes no memory of
        // this crate's, and nothing the caller holds can break.
        Span(unsafe { sys::span_source(self.0) })
    }

    pub fn byte_range(&self) -> Range<usize> {
        let mut out = [0u32; 2];
        // SAFETY: `out` is two writable `u32`s of this frame, the room `sys::span_byte_range`
        // writes, live and exclusive for the call.
        unsafe { sys::span_byte_range(self.0, out.as_mut_ptr()) };
        out[0] as usize..out[1] as usize
    }

    pub fn start(&self) -> Span {
        // SAFETY: the `sys` call takes and gives integers only, so it reads and writes no memory of
        // this crate's, and nothing the caller holds can break.
        Span(unsafe { sys::span_start(self.0) })
    }

    pub fn end(&self) -> Span {
        // SAFETY: the `sys` call takes and gives integers only, so it reads and writes no memory of
        // this crate's, and nothing the caller holds can break.
        Span(unsafe { sys::span_end(self.0) })
    }

    pub fn line(&self) -> usize {
        // SAFETY: the `sys` call takes and gives integers only, so it reads and writes no memory of
        // this crate's, and nothing the caller holds can break.
        unsafe { sys::span_line(self.0) as usize }
    }

    pub fn column(&self) -> usize {
        // SAFETY: the `sys` call takes and gives integers only, so it reads and writes no memory of
        // this crate's, and nothing the caller holds can break.
        unsafe { sys::span_column(self.0) as usize }
    }

    pub fn file(&self) -> String {
        // SAFETY: the `sys` call takes and gives integers only, so it reads and writes no memory of
        // this crate's, and nothing the caller holds can break.
        taken_string(unsafe { sys::span_file(self.0) }).unwrap_or_default()
    }

    pub fn local_file(&self) -> Option<PathBuf> {
        // SAFETY: the `sys` call takes and gives integers only, so it reads and writes no memory of
        // this crate's, and nothing the caller holds can break.
        taken_string(unsafe { sys::span_local_file(self.0) }).map(PathBuf::from)
    }

    pub fn join(&self, other: Span) -> Option<Span> {
        // SAFETY: the `sys` call takes and gives integers only, so it reads and writes no memory of
        // this crate's, and nothing the caller holds can break.
        Span::some(unsafe { sys::span_join(self.0, other.0) })
    }

    pub fn resolved_at(&self, other: Span) -> Span {
        // SAFETY: the `sys` call takes and gives integers only, so it reads and writes no memory of
        // this crate's, and nothing the caller holds can break.
        Span(unsafe { sys::span_resolved_at(self.0, other.0) })
    }

    pub fn located_at(&self, other: Span) -> Span {
        // SAFETY: the `sys` call takes and gives integers only, so it reads and writes no memory of
        // this crate's, and nothing the caller holds can break.
        Span(unsafe { sys::span_located_at(self.0, other.0) })
    }

    pub fn eq(&self, other: &Span) -> bool {
        self.0 == other.0
    }

    pub fn source_text(&self) -> Option<String> {
        // SAFETY: the `sys` call takes and gives integers only, so it reads and writes no memory of
        // this crate's, and nothing the caller holds can break.
        taken_string(unsafe { sys::span_source_text(self.0) })
    }

    pub fn save_span(&self) -> usize {
        0
    }

    pub fn recover_proc_macro_span(_id: usize) -> Span {
        Span::call_site()
    }

    pub fn error<T: Into<String>>(self, message: T) -> Diagnostic {
        Diagnostic::spanned(self, Level::Error, message)
    }

    pub fn warning<T: Into<String>>(self, message: T) -> Diagnostic {
        Diagnostic::spanned(self, Level::Warning, message)
    }

    pub fn note<T: Into<String>>(self, message: T) -> Diagnostic {
        Diagnostic::spanned(self, Level::Note, message)
    }

    pub fn help<T: Into<String>>(self, message: T) -> Diagnostic {
        Diagnostic::spanned(self, Level::Help, message)
    }
}

impl fmt::Debug for Span {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // SAFETY: the `sys` call takes and gives integers only, so it reads and writes no memory of
        // this crate's, and nothing the caller holds can break.
        let text = taken_string(unsafe { sys::span_debug(self.0) }).unwrap_or_default();
        f.write_str(&text)
    }
}

#[derive(Clone)]
pub enum TokenTree {
    Group(Group),
    Ident(Ident),
    Punct(Punct),
    Literal(Literal),
}

impl TokenTree {
    pub fn span(&self) -> Span {
        match self {
            TokenTree::Group(it) => it.span(),
            TokenTree::Ident(it) => it.span(),
            TokenTree::Punct(it) => it.span(),
            TokenTree::Literal(it) => it.span(),
        }
    }

    pub fn set_span(&mut self, span: Span) {
        match self {
            TokenTree::Group(it) => it.set_span(span),
            TokenTree::Ident(it) => it.set_span(span),
            TokenTree::Punct(it) => it.set_span(span),
            TokenTree::Literal(it) => it.set_span(span),
        }
    }
}

impl fmt::Debug for TokenTree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TokenTree::Group(it) => fmt::Debug::fmt(it, f),
            TokenTree::Ident(it) => fmt::Debug::fmt(it, f),
            TokenTree::Punct(it) => fmt::Debug::fmt(it, f),
            TokenTree::Literal(it) => fmt::Debug::fmt(it, f),
        }
    }
}

impl From<Group> for TokenTree {
    fn from(it: Group) -> TokenTree {
        TokenTree::Group(it)
    }
}

impl From<Ident> for TokenTree {
    fn from(it: Ident) -> TokenTree {
        TokenTree::Ident(it)
    }
}

impl From<Punct> for TokenTree {
    fn from(it: Punct) -> TokenTree {
        TokenTree::Punct(it)
    }
}

impl From<Literal> for TokenTree {
    fn from(it: Literal) -> TokenTree {
        TokenTree::Literal(it)
    }
}

impl fmt::Display for TokenTree {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&TokenStream::from(self.clone()), f)
    }
}

#[derive(Clone)]
pub struct Group {
    delimiter: Delimiter,
    stream: TokenStream,
    open: Span,
    close: Span,
    entire: Span,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Delimiter {
    Parenthesis,
    Brace,
    Bracket,
    None,
}

impl Delimiter {
    fn text(self) -> (&'static str, &'static str) {
        match self {
            Delimiter::Parenthesis => ("(", ")"),
            Delimiter::Brace => ("{", "}"),
            Delimiter::Bracket => ("[", "]"),
            Delimiter::None => ("", ""),
        }
    }
}

impl Group {
    pub fn new(delimiter: Delimiter, stream: TokenStream) -> Group {
        let span = Span::call_site();
        Group { delimiter, stream, open: span, close: span, entire: span }
    }

    pub fn delimiter(&self) -> Delimiter {
        self.delimiter
    }

    pub fn stream(&self) -> TokenStream {
        self.stream.clone()
    }

    pub fn span(&self) -> Span {
        self.entire
    }

    pub fn span_open(&self) -> Span {
        self.open
    }

    pub fn span_close(&self) -> Span {
        self.close
    }

    pub fn set_span(&mut self, span: Span) {
        (self.open, self.close, self.entire) = (span, span, span);
    }
}

impl fmt::Display for Group {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&TokenStream::from(TokenTree::Group(self.clone())), f)
    }
}

impl fmt::Debug for Group {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Group")
            .field("delimiter", &self.delimiter)
            .field("stream", &self.stream)
            .field("span", &self.entire)
            .finish()
    }
}

#[derive(Clone)]
pub struct Punct {
    ch: char,
    joint: bool,
    span: Span,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Spacing {
    Joint,
    Alone,
}

impl Punct {
    pub fn new(ch: char, spacing: Spacing) -> Punct {
        const LEGAL_CHARS: &[char] = &[
            '=', '<', '>', '!', '~', '+', '-', '*', '/', '%', '^', '&', '|', '@', '.', ',', ';',
            ':', '#', '$', '?', '\'',
        ];
        if !LEGAL_CHARS.contains(&ch) {
            panic!("unsupported character `{:?}`", ch);
        }
        Punct { ch, joint: spacing == Spacing::Joint, span: Span::call_site() }
    }

    pub fn as_char(&self) -> char {
        self.ch
    }

    pub fn spacing(&self) -> Spacing {
        if self.joint { Spacing::Joint } else { Spacing::Alone }
    }

    pub fn span(&self) -> Span {
        self.span
    }

    pub fn set_span(&mut self, span: Span) {
        self.span = span;
    }
}

impl fmt::Display for Punct {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.ch)
    }
}

impl fmt::Debug for Punct {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Punct")
            .field("ch", &self.ch)
            .field("spacing", &self.spacing())
            .field("span", &self.span)
            .finish()
    }
}

impl PartialEq<char> for Punct {
    fn eq(&self, rhs: &char) -> bool {
        self.ch == *rhs
    }
}

impl PartialEq<Punct> for char {
    fn eq(&self, rhs: &Punct) -> bool {
        *self == rhs.ch
    }
}

/// How an identifier is written: `x`, `r#x` or `k#x`.
#[derive(Copy, Clone, PartialEq, Eq)]
enum IdentKind {
    Normal,
    Raw,
    ForcedKeyword,
}

#[derive(Clone)]
pub struct Ident {
    sym: Rc<str>,
    kind: IdentKind,
    span: Span,
}

impl Ident {
    pub fn new(string: &str, span: Span) -> Ident {
        Ident { sym: string.into(), kind: IdentKind::Normal, span }
    }

    pub fn new_raw(string: &str, span: Span) -> Ident {
        Ident { sym: string.into(), kind: IdentKind::Raw, span }
    }

    pub fn span(&self) -> Span {
        self.span
    }

    pub fn set_span(&mut self, span: Span) {
        self.span = span;
    }
}

impl fmt::Display for Ident {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.kind {
            IdentKind::Normal => {}
            IdentKind::Raw => f.write_str("r#")?,
            IdentKind::ForcedKeyword => f.write_str("k#")?,
        }
        f.write_str(&self.sym)
    }
}

impl fmt::Debug for Ident {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Ident").field("ident", &self.to_string()).field("span", &self.span).finish()
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum LitKind {
    Byte,
    Char,
    Integer,
    Float,
    Str,
    StrRaw(u8),
    ByteStr,
    ByteStrRaw(u8),
    CStr,
    CStrRaw(u8),
    ErrWithGuar,
}

#[derive(Clone)]
pub struct Literal {
    kind: LitKind,
    symbol: Rc<str>,
    suffix: Option<Rc<str>>,
    span: Span,
}

macro_rules! suffixed_int_literals {
    ($($name:ident => $kind:ident,)*) => ($(
        pub fn $name(n: $kind) -> Literal {
            Literal::new(LitKind::Integer, &n.to_string(), Some(stringify!($kind)))
        }
    )*)
}

macro_rules! unsuffixed_int_literals {
    ($($name:ident => $kind:ident,)*) => ($(
        pub fn $name(n: $kind) -> Literal {
            Literal::new(LitKind::Integer, &n.to_string(), None)
        }
    )*)
}

impl Literal {
    fn new(kind: LitKind, value: &str, suffix: Option<&str>) -> Literal {
        Literal {
            kind,
            symbol: value.into(),
            suffix: suffix.map(Into::into),
            span: Span::call_site(),
        }
    }

    suffixed_int_literals! {
        u8_suffixed => u8,
        u16_suffixed => u16,
        u32_suffixed => u32,
        u64_suffixed => u64,
        u128_suffixed => u128,
        usize_suffixed => usize,
        i8_suffixed => i8,
        i16_suffixed => i16,
        i32_suffixed => i32,
        i64_suffixed => i64,
        i128_suffixed => i128,
        isize_suffixed => isize,
    }

    unsuffixed_int_literals! {
        u8_unsuffixed => u8,
        u16_unsuffixed => u16,
        u32_unsuffixed => u32,
        u64_unsuffixed => u64,
        u128_unsuffixed => u128,
        usize_unsuffixed => usize,
        i8_unsuffixed => i8,
        i16_unsuffixed => i16,
        i32_unsuffixed => i32,
        i64_unsuffixed => i64,
        i128_unsuffixed => i128,
        isize_unsuffixed => isize,
    }

    pub fn f32_unsuffixed(n: f32) -> Literal {
        if !n.is_finite() {
            panic!("Invalid float literal {n}");
        }
        let mut repr = n.to_string();
        if !repr.contains('.') {
            repr.push_str(".0");
        }
        Literal::new(LitKind::Float, &repr, None)
    }

    pub fn f32_suffixed(n: f32) -> Literal {
        if !n.is_finite() {
            panic!("Invalid float literal {n}");
        }
        Literal::new(LitKind::Float, &n.to_string(), Some("f32"))
    }

    pub fn f64_unsuffixed(n: f64) -> Literal {
        if !n.is_finite() {
            panic!("Invalid float literal {n}");
        }
        let mut repr = n.to_string();
        if !repr.contains('.') {
            repr.push_str(".0");
        }
        Literal::new(LitKind::Float, &repr, None)
    }

    pub fn f64_suffixed(n: f64) -> Literal {
        if !n.is_finite() {
            panic!("Invalid float literal {n}");
        }
        Literal::new(LitKind::Float, &n.to_string(), Some("f64"))
    }

    pub fn string(string: &str) -> Literal {
        let escape = EscapeOptions {
            escape_single_quote: false,
            escape_double_quote: true,
            escape_nonascii: false,
        };
        Literal::new(LitKind::Str, &escape_bytes(string.as_bytes(), escape), None)
    }

    pub fn character(ch: char) -> Literal {
        let escape = EscapeOptions {
            escape_single_quote: true,
            escape_double_quote: false,
            escape_nonascii: false,
        };
        let repr = escape_bytes(ch.encode_utf8(&mut [0u8; 4]).as_bytes(), escape);
        Literal::new(LitKind::Char, &repr, None)
    }

    pub fn byte_character(byte: u8) -> Literal {
        let escape = EscapeOptions {
            escape_single_quote: true,
            escape_double_quote: false,
            escape_nonascii: true,
        };
        Literal::new(LitKind::Byte, &escape_bytes(&[byte], escape), None)
    }

    pub fn byte_string(bytes: &[u8]) -> Literal {
        let escape = EscapeOptions {
            escape_single_quote: false,
            escape_double_quote: true,
            escape_nonascii: true,
        };
        Literal::new(LitKind::ByteStr, &escape_bytes(bytes, escape), None)
    }

    pub fn c_string(string: &CStr) -> Literal {
        let escape = EscapeOptions {
            escape_single_quote: false,
            escape_double_quote: true,
            escape_nonascii: false,
        };
        Literal::new(LitKind::CStr, &escape_bytes(string.to_bytes(), escape), None)
    }

    pub fn span(&self) -> Span {
        self.span
    }

    pub fn set_span(&mut self, span: Span) {
        self.span = span;
    }

    pub fn subspan<R: RangeBounds<usize>>(&self, range: R) -> Option<Span> {
        // Each bound as 0 for none, else 1 + 2 * whether it is excluded + 4 * its value.
        let bound = |it: Bound<&usize>| match it {
            Bound::Unbounded => 0u64,
            Bound::Included(at) => 1 + 4 * *at as u64,
            Bound::Excluded(at) => 3 + 4 * *at as u64,
        };
        let (start, end) = (bound(range.start_bound()), bound(range.end_bound()));
        // SAFETY: the `sys` call takes and gives integers only, so it reads and writes no memory of
        // this crate's, and nothing the caller holds can break.
        Span::some(unsafe { sys::span_subspan(self.span.0, start, end) })
    }

    fn parts(&self, f: &mut dyn FnMut(&str) -> fmt::Result) -> fmt::Result {
        let hashes = |n: u8| "#".repeat(n as usize);
        let symbol = &*self.symbol;
        match self.kind {
            LitKind::Byte => [f("b'"), f(symbol), f("'")].into_iter().collect::<fmt::Result>()?,
            LitKind::Char => [f("'"), f(symbol), f("'")].into_iter().collect::<fmt::Result>()?,
            LitKind::Str => [f("\""), f(symbol), f("\"")].into_iter().collect::<fmt::Result>()?,
            LitKind::StrRaw(n) => {
                let h = hashes(n);
                [f("r"), f(&h), f("\""), f(symbol), f("\""), f(&h)]
                    .into_iter()
                    .collect::<fmt::Result>()?
            }
            LitKind::ByteStr => {
                [f("b\""), f(symbol), f("\"")].into_iter().collect::<fmt::Result>()?
            }
            LitKind::ByteStrRaw(n) => {
                let h = hashes(n);
                [f("br"), f(&h), f("\""), f(symbol), f("\""), f(&h)]
                    .into_iter()
                    .collect::<fmt::Result>()?
            }
            LitKind::CStr => [f("c\""), f(symbol), f("\"")].into_iter().collect::<fmt::Result>()?,
            LitKind::CStrRaw(n) => {
                let h = hashes(n);
                [f("cr"), f(&h), f("\""), f(symbol), f("\""), f(&h)]
                    .into_iter()
                    .collect::<fmt::Result>()?
            }
            LitKind::Integer | LitKind::Float | LitKind::ErrWithGuar => f(symbol)?,
        }
        match &self.suffix {
            Some(suffix) => f(suffix),
            None => Ok(()),
        }
    }
}

impl FromStr for Literal {
    type Err = LexError;

    fn from_str(src: &str) -> Result<Literal, LexError> {
        // SAFETY: `src` is borrowed for the call, so its `src.len()` bytes at `src.as_ptr()` stay
        // live and unchanged; the host only reads them, during the call, and copies what it keeps.
        let len = unsafe { sys::literal_from_str(src.as_ptr(), src.len()) };
        let Some(bytes) = taken(len) else {
            return Err(LexError("cannot parse string into literal".to_owned()));
        };
        match wire::Reader::new(&bytes).contents().0.first() {
            Some(TokenTree::Literal(literal)) => Ok(literal.clone()),
            _ => Err(LexError("cannot parse string into literal".to_owned())),
        }
    }
}

impl fmt::Display for Literal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.parts(&mut |part| f.write_str(part))
    }
}

impl fmt::Debug for Literal {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Literal")
            .field("kind", &format_args!("{:?}", self.kind))
            .field("symbol", &self.symbol)
            .field("suffix", &format_args!("{:?}", self.suffix))
            .field("span", &self.span)
            .finish()
    }
}

#[derive(Copy, Clone)]
struct EscapeOptions {
    escape_single_quote: bool,
    escape_double_quote: bool,
    escape_nonascii: bool,
}

fn escape_bytes(bytes: &[u8], opt: EscapeOptions) -> String {
    let mut repr = String::new();
    if opt.escape_nonascii {
        for &byte in bytes {
            escape_single_byte(byte, opt, &mut repr);
        }
    } else {
        for chunk in bytes.utf8_chunks() {
            for ch in chunk.valid().chars() {
                if (ch == '\'' && !opt.escape_single_quote)
                    || (ch == '"' && !opt.escape_double_quote)
                {
                    repr.push(ch);
                } else {
                    repr.extend(ch.escape_debug());
                }
            }
            for &byte in chunk.invalid() {
                escape_single_byte(byte, opt, &mut repr);
            }
        }
    }
    repr
}

fn escape_single_byte(byte: u8, opt: EscapeOptions, repr: &mut String) {
    if byte == b'\0' {
        repr.push_str("\\0");
    } else if (byte == b'\'' && !opt.escape_single_quote)
        || (byte == b'"' && !opt.escape_double_quote)
    {
        repr.push(byte as char);
    } else {
        repr.extend(byte.escape_ascii().map(char::from));
    }
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Level {
    Error,
    Warning,
    Note,
    Help,
}

pub trait MultiSpan {
    fn into_spans(self) -> Vec<Span>;
}

impl MultiSpan for Span {
    fn into_spans(self) -> Vec<Span> {
        vec![self]
    }
}

impl MultiSpan for Vec<Span> {
    fn into_spans(self) -> Vec<Span> {
        self
    }
}

impl<'a> MultiSpan for &'a [Span] {
    fn into_spans(self) -> Vec<Span> {
        self.to_vec()
    }
}

#[derive(Clone, Debug)]
pub struct Diagnostic {
    level: Level,
    message: String,
    spans: Vec<Span>,
    children: Vec<Diagnostic>,
}

macro_rules! diagnostic_child_methods {
    ($spanned:ident, $regular:ident, $level:expr) => {
        pub fn $spanned<S, T>(mut self, spans: S, message: T) -> Diagnostic
        where
            S: MultiSpan,
            T: Into<String>,
        {
            self.children.push(Diagnostic::spanned(spans, $level, message));
            self
        }

        pub fn $regular<T: Into<String>>(mut self, message: T) -> Diagnostic {
            self.children.push(Diagnostic::new($level, message));
            self
        }
    };
}

impl Diagnostic {
    pub fn new<T: Into<String>>(level: Level, message: T) -> Diagnostic {
        Diagnostic { level, message: message.into(), spans: vec![], children: vec![] }
    }

    pub fn spanned<S, T>(spans: S, level: Level, message: T) -> Diagnostic
    where
        S: MultiSpan,
        T: Into<String>,
    {
        Diagnostic { level, message: message.into(), spans: spans.into_spans(), children: vec![] }
    }

    diagnostic_child_methods!(span_error, error, Level::Error);
    diagnostic_child_methods!(span_warning, warning, Level::Warning);
    diagnostic_child_methods!(span_note, note, Level::Note);
    diagnostic_child_methods!(span_help, help, Level::Help);

    pub fn level(&self) -> Level {
        self.level
    }

    pub fn set_level(&mut self, level: Level) {
        self.level = level;
    }

    pub fn message(&self) -> &str {
        &self.message
    }

    pub fn set_message<T: Into<String>>(&mut self, message: T) {
        self.message = message.into();
    }

    pub fn spans(&self) -> &[Span] {
        &self.spans
    }

    pub fn set_spans<S: MultiSpan>(&mut self, spans: S) {
        self.spans = spans.into_spans();
    }

    pub fn children(&self) -> impl Iterator<Item = &Diagnostic> {
        self.children.iter()
    }

    pub fn emit(self) {
        let level = match self.level {
            Level::Error => 0,
            Level::Warning => 1,
            Level::Note => 2,
            Level::Help => 3,
        };
        let span = self.spans.first().map_or(Span::NONE, |it| it.0);
        // SAFETY: `self.message` is borrowed for the call, so its `self.message.len()` bytes at
        // `self.message.as_ptr()` stay live and unchanged; the host only reads them, during the
        // call, and copies what it keeps.
        unsafe { sys::diagnostic(level, self.message.as_ptr(), self.message.len(), span) };
        for child in self.children {
            child.emit();
        }
    }
}

pub mod tracked {
    use std::{
        env::{self, VarError},
        ffi::OsStr,
        path::Path,
    };

    pub fn env_var<K: AsRef<OsStr> + AsRef<str>>(key: K) -> Result<String, VarError> {
        let key: &str = key.as_ref();
        // SAFETY: `key` is borrowed for the call, so its `key.len()` bytes at `key.as_ptr()` stay
        // live and unchanged; the host only reads them, during the call, and copies what it keeps.
        unsafe { super::sys::track_env_var(key.as_ptr(), key.len()) };
        env::var(key)
    }

    pub fn path<P: AsRef<Path>>(path: P) {
        let path: &str = path.as_ref().to_str().unwrap();
        // SAFETY: `path` is borrowed for the call, so its `path.len()` bytes at `path.as_ptr()`
        // stay live and unchanged; the host only reads them, during the call, and copies what it
        // keeps.
        unsafe { super::sys::track_path(path.as_ptr(), path.len()) };
    }
}

/// The encoding of a stream between the module and the host: the columns blaze keeps a token
/// tree in, as it lays them out on a wire (`crates/tt/src/storage.rs`, `storage/wire.rs`), in
/// little-endian `u32` words:
///
/// ```text
/// tree    := header node* span* wide* part* end* top top text padding
/// header  := MAGIC nodes wide parts symbols text_len
/// node    := head data      (the top subtree first, then each token tree in preorder)
/// span    := start len_part (each node's, the top's unused, then each subtree's closing one)
/// wide    := start len part
/// part    := index 0 0      (an index of the expansion's span parts)
/// end     := where a symbol's text ends in `text`
/// top     := start len index 0 0
/// ```
///
/// A head's low two bits are its node's kind: 0 a subtree (its delimiter, then its ordinal
/// from bit 4; `data` how many nodes it holds), 1 a punct (its spacing; `data` the char), 2 an
/// ident (whether raw; `data` its symbol), 3 a literal (its kind, a raw one's hashes from bit 6
/// and its suffix's length from bit 14; `data` its symbol, the text then the suffix). A span's
/// `len_part` is `len | part << 16` with `part` an index of the tree's parts, or `u32::MAX`
/// with `start` an index of its wide spans. A [`Span`] packs the same way with an index of the
/// expansion's parts, so one crosses with arithmetic alone unless it is too wide to pack. What
/// a module writes is what blaze's builder makes of the stream, which the host checks.
pub mod wire {
    use std::rc::Rc;

    use super::*;

    const MAGIC: u32 = u32::from_le_bytes(*b"tt\x01\x00");
    const HEADER: usize = 6;
    /// The `len_part` of a span whose `start` is an index of the wide spans.
    const WIDE: u32 = u32::MAX;
    /// What a packed span's length and part each stay under.
    const NARROW: u32 = 0xFFFF;
    /// How many of the latest symbols a new one is looked for among.
    const SYMBOL_WINDOW: usize = 16;
    const SUFFIX_SHIFT: u32 = 14;
    /// Below this, a writer finds an expansion part's index among its own by indexing, not by
    /// a scan.
    const DIRECT: u32 = 1 << 20;

    /// The span `start..start + len` in the expansion's part `part`.
    fn span(start: u32, len: u32, part: u32) -> Span {
        if len < NARROW && part < NARROW {
            return Span(u64::from(start) | u64::from(len | part << 16) << 32);
        }
        // SAFETY: the `sys` call takes and gives integers only, so it reads and writes no
        // memory of this crate's and nothing the caller holds can break.
        Span(unsafe { sys::span_pack(start, len, part) })
    }

    /// `[start, len, part]` of `span`, `part` an index of the expansion's parts.
    fn fields(span: Span) -> [u32; 3] {
        let (start, len_part) = (span.0 as u32, (span.0 >> 32) as u32);
        if len_part != WIDE {
            return [start, len_part & NARROW, len_part >> 16];
        }
        let mut out = [0u32; 3];
        // SAFETY: `out` is three writable `u32`s of this frame, the room `sys::span_unpack`
        // writes, live and not borrowed elsewhere for the call.
        unsafe { sys::span_unpack(span.0, out.as_mut_ptr()) };
        out
    }

    /// What rustc calls a whole group's span, which a token tree doesn't keep: the cover of its
    /// delimiters' when they have one anchor and context, else the opening one's, as
    /// rust-analyzer's server joins them.
    pub(crate) fn entire(open: Span, close: Span) -> Span {
        let [open_start, open_len, part] = fields(open);
        let [close_start, close_len, close_part] = fields(close);
        if part != close_part {
            return open;
        }
        let start = open_start.min(close_start);
        let end = open_start.saturating_add(open_len).max(close_start.saturating_add(close_len));
        span(start, end - start, part)
    }

    fn u32_at(bytes: &[u8], at: usize) -> u32 {
        u32::from_le_bytes(bytes[at..at + 4].try_into().unwrap())
    }

    fn delimiter_of(code: u32) -> Delimiter {
        match code & 0b11 {
            0 => Delimiter::Parenthesis,
            1 => Delimiter::Brace,
            2 => Delimiter::Bracket,
            _ => Delimiter::None,
        }
    }

    fn delimiter_code(delimiter: Delimiter) -> u32 {
        match delimiter {
            Delimiter::Parenthesis => 0,
            Delimiter::Brace => 1,
            Delimiter::Bracket => 2,
            Delimiter::None => 3,
        }
    }

    fn lit_kind(code: u32, hashes: u8) -> LitKind {
        match code {
            0 => LitKind::Byte,
            1 => LitKind::Char,
            2 => LitKind::Integer,
            3 => LitKind::Float,
            4 => LitKind::Str,
            5 => LitKind::StrRaw(hashes),
            6 => LitKind::ByteStr,
            7 => LitKind::ByteStrRaw(hashes),
            8 => LitKind::CStr,
            9 => LitKind::CStrRaw(hashes),
            _ => LitKind::ErrWithGuar,
        }
    }

    fn lit_code(kind: LitKind) -> (u32, u32) {
        match kind {
            LitKind::Byte => (0, 0),
            LitKind::Char => (1, 0),
            LitKind::Integer => (2, 0),
            LitKind::Float => (3, 0),
            LitKind::Str => (4, 0),
            LitKind::StrRaw(n) => (5, u32::from(n)),
            LitKind::ByteStr => (6, 0),
            LitKind::ByteStrRaw(n) => (7, u32::from(n)),
            LitKind::CStr => (8, 0),
            LitKind::CStrRaw(n) => (9, u32::from(n)),
            LitKind::ErrWithGuar => (10, 0),
        }
    }

    /// A tree the host wrote, read where its bytes lie; no bytes are a tree of nothing.
    pub struct Reader<'a> {
        bytes: &'a [u8],
        /// How many nodes there are, the top subtree's included.
        len: usize,
        spans: usize,
        wide: usize,
        parts: usize,
        ends: usize,
        top: usize,
        text: usize,
    }

    impl<'a> Reader<'a> {
        pub fn new(bytes: &'a [u8]) -> Reader<'a> {
            if bytes.is_empty() {
                return Reader { bytes, len: 0, spans: 0, wide: 0, parts: 0, ends: 0, top: 0, text: 0 };
            }
            let word = |at: usize| u32_at(bytes, 4 * at) as usize;
            let (len, wide, parts, symbols) = (word(1), word(2), word(3), word(4));
            let subtrees = (1..len).filter(|&node| word(HEADER + 2 * node) & 0b11 == 0).count();
            let spans = 4 * (HEADER + 2 * len);
            let wide_at = spans + 8 * (len + subtrees);
            let parts_at = wide_at + 12 * wide;
            let ends = parts_at + 12 * parts;
            let top = ends + 4 * symbols;
            Reader { bytes, len, spans, wide: wide_at, parts: parts_at, ends, top, text: top + 40 }
        }

        fn node(&self, at: usize) -> (u32, u32) {
            let it = 4 * (HEADER + 2 * at);
            (u32_at(self.bytes, it), u32_at(self.bytes, it + 4))
        }

        /// The span at `at` of the span column.
        fn span(&self, at: usize) -> Span {
            let it = self.spans + 8 * at;
            let (start, len_part) = (u32_at(self.bytes, it), u32_at(self.bytes, it + 4));
            let [start, len, local] = match len_part {
                WIDE => {
                    let it = self.wide + 12 * start as usize;
                    [u32_at(self.bytes, it), u32_at(self.bytes, it + 4), u32_at(self.bytes, it + 8)]
                }
                _ => [start, len_part & NARROW, len_part >> 16],
            };
            span(start, len, u32_at(self.bytes, self.parts + 12 * local as usize))
        }

        /// The closing span of the subtree whose head is `head`.
        fn close(&self, head: u32) -> Span {
            self.span(self.len + (head >> 4) as usize - 1)
        }

        /// The top subtree's opening span for 0, its closing one for 1.
        fn top_span(&self, which: usize) -> Span {
            let it = self.top + 20 * which;
            span(u32_at(self.bytes, it), u32_at(self.bytes, it + 4), u32_at(self.bytes, it + 8))
        }

        fn symbol(&self, index: u32) -> &'a str {
            let bytes = self.bytes;
            let index = index as usize;
            let start = match index {
                0 => 0,
                _ => u32_at(bytes, self.ends + 4 * (index - 1)) as usize,
            };
            let end = u32_at(bytes, self.ends + 4 * index) as usize;
            std::str::from_utf8(&bytes[self.text + start..self.text + end]).unwrap()
        }

        /// What a macro is given of the tree: what its top subtree holds when that has no
        /// delimiters, else the top subtree, without the parentheses rust-analyzer's protocol
        /// wraps a passed-on fragment in (see [`Reader::inside`]).
        pub fn input(&self) -> TokenStream {
            if self.len == 0 {
                return TokenStream::new();
            }
            let delimiter = delimiter_of(self.node(0).0 >> 2);
            let (open, close) = (self.top_span(0), self.top_span(1));
            if delimiter == Delimiter::None {
                return self.inside(0, open, delimiter, true);
            }
            let entire = entire(open, close);
            let stream = self.inside(0, open, delimiter, true);
            TokenStream(Rc::new(vec![TokenTree::Group(Group { delimiter, stream, open, close, entire })]))
        }

        /// What the top subtree holds, as it is: the tokens of text a macro asked for.
        pub fn contents(&self) -> TokenStream {
            if self.len == 0 {
                return TokenStream::new();
            }
            self.trees(1, self.len, false)
        }

        /// What the subtree at `at` holds; with `fragments`, the contents of the parentheses
        /// when it is a group without delimiters around exactly a pair of them whose spans are
        /// both its opening one: how rust-analyzer's protocol wraps an expression a macro was
        /// passed and passes on, which a proc macro is not to see.
        fn inside(&self, at: usize, open: Span, delimiter: Delimiter, fragments: bool) -> TokenStream {
            let end = at + 1 + self.node(at).1 as usize;
            if fragments && delimiter == Delimiter::None && at + 1 < end {
                let (head, len) = self.node(at + 1);
                let wrapper = head & 0b11 == 0
                    && delimiter_of(head >> 2) == Delimiter::Parenthesis
                    && at + 2 + len as usize == end
                    && self.span(at + 1) == open
                    && self.close(head) == open;
                if wrapper {
                    return self.trees(at + 2, end, fragments);
                }
            }
            self.trees(at + 1, end, fragments)
        }

        fn trees(&self, mut at: usize, end: usize, fragments: bool) -> TokenStream {
            let mut trees = Vec::new();
            while at < end {
                let (head, data) = self.node(at);
                let tree = match head & 0b11 {
                    0 => {
                        let delimiter = delimiter_of(head >> 2);
                        let (open, close) = (self.span(at), self.close(head));
                        let entire = entire(open, close);
                        let stream = self.inside(at, open, delimiter, fragments);
                        at += data as usize;
                        TokenTree::Group(Group { delimiter, stream, open, close, entire })
                    }
                    1 => TokenTree::Punct(Punct {
                        ch: char::from_u32(data).unwrap_or('?'),
                        joint: (head >> 2) & 0b11 == 1,
                        span: self.span(at),
                    }),
                    2 => TokenTree::Ident(Ident {
                        sym: self.symbol(data).into(),
                        kind: if (head >> 2) & 1 == 1 { IdentKind::Raw } else { IdentKind::Normal },
                        span: self.span(at),
                    }),
                    _ => {
                        let text = self.symbol(data);
                        let (symbol, suffix) =
                            text.split_at(text.len() - (head >> SUFFIX_SHIFT) as usize);
                        TokenTree::Literal(Literal {
                            kind: lit_kind((head >> 2) & 0xF, (head >> 6) as u8),
                            symbol: symbol.into(),
                            suffix: (!suffix.is_empty()).then(|| suffix.into()),
                            span: self.span(at),
                        })
                    }
                };
                trees.push(tree);
                at += 1;
            }
            TokenStream(Rc::new(trees))
        }
    }

    /// Writes `stream` as the columns blaze's builder makes of it: its one group as the top
    /// subtree, else a top without delimiters at the call site; a negative number as a minus
    /// sign and the number, as blaze's lexer has one; each node's span then a subtree's closing
    /// one, the tree's parts and wide spans in the order first used; a symbol found again
    /// among the latest [`SYMBOL_WINDOW`].
    pub fn write(stream: &TokenStream, out: &mut Vec<u8>) {
        let (delimiter, open, close, stream) = match &stream.0[..] {
            [TokenTree::Group(group)] => (group.delimiter, group.open, group.close, &group.stream),
            _ => {
                let at = Span::call_site();
                (Delimiter::None, at, at, stream)
            }
        };
        let mut writer = Writer::default();
        writer.nodes.extend([delimiter_code(delimiter) << 2, 0]);
        writer.spans.extend([0, 0]);
        writer.stream(stream);
        let len = writer.nodes.len() / 2;
        writer.nodes[1] = (len - 1) as u32;
        let put = |out: &mut Vec<u8>, words: &[u32]| {
            for word in words {
                out.extend_from_slice(&word.to_le_bytes());
            }
        };
        let base = out.len();
        put(out, &[
            MAGIC,
            len as u32,
            (writer.wide.len() / 3) as u32,
            writer.parts.len() as u32,
            writer.ends.len() as u32,
            writer.text.len() as u32,
        ]);
        put(out, &writer.nodes);
        put(out, &writer.spans);
        put(out, &writer.closes);
        put(out, &writer.wide);
        for &part in &writer.parts {
            put(out, &[part, 0, 0]);
        }
        put(out, &writer.ends);
        for span in [open, close] {
            let [start, len, part] = fields(span);
            put(out, &[start, len, part, 0, 0]);
        }
        out.extend_from_slice(&writer.text);
        out.resize(base + (out.len() - base).next_multiple_of(4), 0);
    }

    #[derive(Default)]
    struct Writer {
        nodes: Vec<u32>,
        spans: Vec<u32>,
        closes: Vec<u32>,
        wide: Vec<u32>,
        /// The expansion's index of each of the tree's parts, in the order first used.
        parts: Vec<u32>,
        /// One more than the tree's index of each expansion part below [`DIRECT`], 0 for one
        /// not used yet.
        local: Vec<u32>,
        last: Option<(u32, u32)>,
        ends: Vec<u32>,
        text: Vec<u8>,
    }

    impl Writer {
        /// The tree's index of the expansion's part `part`, given one the first time.
        fn local(&mut self, part: u32) -> u32 {
            match self.last {
                Some((last, local)) if last == part => return local,
                _ => {}
            }
            let local = if part < DIRECT {
                let at = part as usize;
                if self.local.len() <= at {
                    self.local.resize(at + 1, 0);
                }
                if self.local[at] == 0 {
                    self.parts.push(part);
                    self.local[at] = self.parts.len() as u32;
                }
                self.local[at] - 1
            } else {
                match self.parts.iter().position(|&it| it == part) {
                    Some(at) => at as u32,
                    None => {
                        self.parts.push(part);
                        (self.parts.len() - 1) as u32
                    }
                }
            };
            self.last = Some((part, local));
            local
        }

        fn span(&mut self, span: Span) -> [u32; 2] {
            let [start, len, part] = fields(span);
            let local = self.local(part);
            if len < NARROW && local < NARROW {
                return [start, len | local << 16];
            }
            self.wide.extend([start, len, local]);
            [(self.wide.len() / 3 - 1) as u32, WIDE]
        }

        /// The index of the symbol `text` then `suffix`, among the latest [`SYMBOL_WINDOW`] when
        /// there.
        fn symbol(&mut self, text: &str, suffix: &str) -> u32 {
            let count = self.ends.len();
            for index in (count.saturating_sub(SYMBOL_WINDOW)..count).rev() {
                let start = match index {
                    0 => 0,
                    _ => self.ends[index - 1] as usize,
                };
                let it = &self.text[start..self.ends[index] as usize];
                if it.len() == text.len() + suffix.len()
                    && it.starts_with(text.as_bytes())
                    && it.ends_with(suffix.as_bytes())
                {
                    return index as u32;
                }
            }
            self.text.extend_from_slice(text.as_bytes());
            self.text.extend_from_slice(suffix.as_bytes());
            self.ends.push(self.text.len() as u32);
            count as u32
        }

        fn leaf(&mut self, span: Span, head: u32, data: u32) {
            let span = self.span(span);
            self.spans.extend(span);
            self.nodes.extend([head, data]);
        }

        fn stream(&mut self, stream: &TokenStream) {
            for tree in stream.0.iter() {
                match tree {
                    TokenTree::Group(group) => {
                        let at = self.nodes.len();
                        let open = self.span(group.open);
                        let close = self.span(group.close);
                        self.spans.extend(open);
                        self.closes.extend(close);
                        let ordinal = (self.closes.len() / 2) as u32;
                        self.nodes.extend([delimiter_code(group.delimiter) << 2 | ordinal << 4, 0]);
                        self.stream(&group.stream);
                        self.nodes[at + 1] = ((self.nodes.len() - at) / 2 - 1) as u32;
                    }
                    TokenTree::Punct(punct) => {
                        self.leaf(punct.span, 1 | u32::from(punct.joint) << 2, punct.ch as u32)
                    }
                    TokenTree::Ident(ident) => {
                        let symbol = self.symbol(&ident.sym, "");
                        let raw = u32::from(ident.kind == IdentKind::Raw);
                        self.leaf(ident.span, 2 | raw << 2, symbol);
                    }
                    TokenTree::Literal(literal) => {
                        let mut text = &*literal.symbol;
                        let number = matches!(literal.kind, LitKind::Integer | LitKind::Float);
                        if number && text.starts_with('-') {
                            self.leaf(literal.span, 1, '-' as u32);
                            text = &text[1..];
                        }
                        let suffix = literal.suffix.as_deref().unwrap_or("");
                        let symbol = self.symbol(text, suffix);
                        let (code, hashes) = lit_code(literal.kind);
                        let head = 3 | code << 2 | hashes << 6 | (suffix.len() as u32) << SUFFIX_SHIFT;
                        self.leaf(literal.span, head, symbol);
                    }
                }
            }
        }
    }
}

/// What blaze's list of a crate's macros calls (`crates/blaze-engine/src/macro_list.rs`): the
/// module's exports run a macro through these.
#[doc(hidden)]
pub mod __blaze {
    use super::*;

    /// Room for the host to write an input into, kept until the expansion ends.
    pub fn alloc(len: usize) -> *mut u8 {
        let mut room = vec![0u8; len];
        let at = room.as_mut_ptr();
        ROOM.with(|it| it.borrow_mut().push(room));
        at
    }

    thread_local! {
        static ROOM: RefCell<Vec<Vec<u8>>> = const { RefCell::new(Vec::new()) };
    }

    /// The `len` bytes the host wrote at `ptr`, found among the room [`alloc`] gave out rather
    /// than read through the pointer, so a caller of the safe `expand1` or `expand2` can't
    /// make it read memory that is not an input. A length of 0 is no bytes, found nowhere: the
    /// host passes a null pointer with it when there is no attribute.
    fn input(ptr: *const u8, len: usize) -> TokenStream {
        if len == 0 {
            return TokenStream::new();
        }
        ROOM.with(|room| {
            let room = room.borrow();
            let Some(bytes) = room.iter().find(|it| std::ptr::eq(it.as_ptr(), ptr)) else {
                panic!("the host gave an input at {ptr:?}, which is no room `alloc` gave out");
            };
            let Some(bytes) = bytes.get(..len) else {
                panic!("the host gave an input of {len} bytes in room of {}", bytes.len());
            };
            wire::Reader::new(bytes).input()
        })
    }

    fn hook() {
        std::panic::set_hook(Box::new(|info| {
            let payload = info.payload();
            let message = match (payload.downcast_ref::<&str>(), payload.downcast_ref::<String>()) {
                (Some(it), _) => (*it).to_owned(),
                (_, Some(it)) => it.clone(),
                _ => "Box<dyn Any>".to_owned(),
            };
            // SAFETY: `message` is borrowed for the call, so its `message.len()` bytes at
            // `message.as_ptr()` stay live and unchanged; the host only reads them, during the
            // call, and copies what it keeps.
            unsafe { sys::panicked(message.as_ptr(), message.len()) };
        }));
    }

    fn give(output: TokenStream) {
        let mut out = Vec::new();
        wire::write(&output, &mut out);
        // SAFETY: `out` is borrowed for the call, so its `out.len()` bytes at `out.as_ptr()` stay
        // live and unchanged; the host only reads them, during the call, and copies what it keeps.
        unsafe { sys::output(out.as_ptr(), out.len()) };
    }

    /// What is live before an expansion, less the room its input was written into, which it
    /// frees: what is live after it beyond that, it left for a later one.
    fn before() -> usize {
        live() - ROOM.with(|room| room.borrow().iter().map(Vec::capacity).sum::<usize>())
    }

    fn after(before: usize) {
        ROOM.with(|room| room.borrow_mut().clear());
        let left = live().saturating_sub(before);
        if left != 0 {
            // SAFETY: the `sys` call takes and gives integers only, so it reads and writes no
            // memory of this crate's, and nothing the caller holds can break.
            unsafe { sys::retained(left) };
        }
    }

    /// A derive or a function-like macro.
    pub fn expand1(f: fn(TokenStream) -> TokenStream, ptr: *const u8, len: usize) {
        hook();
        let before = before();
        give(f(input(ptr, len)));
        after(before);
    }

    /// An attribute.
    pub fn expand2(
        f: fn(TokenStream, TokenStream) -> TokenStream,
        attr: *const u8,
        attr_len: usize,
        ptr: *const u8,
        len: usize,
    ) {
        hook();
        let before = before();
        give(f(input(attr, attr_len), input(ptr, len)));
        after(before);
    }

    /// The list of macros, a line each: kind (0 derive, 1 attribute, 2 function-like), name
    /// and the derive's helper attributes, tab-separated.
    pub fn list(text: &str) {
        // SAFETY: `text` is borrowed for the call, so its `text.len()` bytes at `text.as_ptr()`
        // stay live and unchanged; the host only reads them, during the call, and copies what it
        // keeps.
        unsafe { sys::output(text.as_ptr(), text.len()) };
    }

    /// As [`list`], of the bytes of the static the module also carries in a custom section.
    pub fn list_bytes(text: &[u8]) {
        // SAFETY: `text` is borrowed for the call, so its `text.len()` bytes at `text.as_ptr()`
        // stay live and unchanged; the host only reads them, during the call, and copies what it
        // keeps.
        unsafe { sys::output(text.as_ptr(), text.len()) };
    }
}
