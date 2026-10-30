
## dedyn: crates/blaze-expand/src/native/: the in-process host is a concrete type, `dyn` only for what asks the database (user's call, 2026-10-29)

Why: `__native::HOST` holds `*mut dyn Host` and every `sys` function, every `word` (one `pack`
per input token, `registered`/`stream`) and every `unpacked` (one `get` per output token) goes
through its vtable; `quote!`/`syn` call `Span::call_site`, `resolved_at` and `located_at` per
token they make. There is one implementation (`native/host.rs`). The `proc_macro` crate can't name
it because blaze-expand depends on it, but everything a per-token call touches is `Parts` and the
three site words, which need nothing of blaze-expand. So: the host becomes a concrete struct
of the `proc_macro` crate owning `Parts` and the sites, and only what needs the database or the
lexer (line/column, file, source text, parent, source, byte range, join, subspan, text to tokens)
crosses a `dyn Ask` that blaze-expand implements. That `dyn` is a real boundary (the crate cycle)
and off the per-token path. No code-size cost: nothing becomes generic. The wasm guest
(`blaze-macro-wasm/guest/proc_macro.rs`) already calls the host through static wasm imports and
needs no change; the wasm host keeps reading `Parts` through `bridge::wire`'s re-export.
dedup-macros (DONE) merged these files; told it.

> forwarded to usercode (owns `crates/blaze-expand/`, lane not DONE; told by tmux; requests, pass 22).
> taken by usercode: a child, `usercode-dedyn-native`, applies parts 1-5 once usercode-hot-macros (holding `native/` for the hot lint) is done.

### 1. `Parts` moves into the `proc_macro` crate

- `crates/blaze-expand/src/native/proc_macro/Cargo.toml` `[dependencies]`: add
  `rustc-hash.workspace = true`.
- Move `crates/blaze-expand/src/bridge/wire.rs` lines 17–193 verbatim (from `/// The word of no
  span` through the end of `impl Parts`, i.e. `NONE`, `WIDE`, `NARROW`, `SCAN`, `Part`, `Parts`
  and its `impl`) into `proc_macro/lib.rs`'s `pub mod __native`, after its `use`s, with these
  three visibility changes, since `wire.rs`'s reader uses them:
  `const WIDE` → `pub const WIDE`, `const NARROW` → `pub const NARROW`,
  `fn part_written` → `pub fn part_written` (each gets `#[doc(hidden)]`). Add to `__native`:
  `use rustc_hash::FxHashMap;` and
  `use span::{EditionedFileId, ErasedFileAstId, SpanAnchor, SyntaxContext, TextRange, TextSize};`
  plus `use tt::WirePart;` (the moved block names them; `span::Span` is already in scope as
  `span::Span` there, so in the moved block write `span::Span` where it says `Span`, because
  `__native`'s `Span` is the macro's).
- In `wire.rs`, in place of the moved lines:

```rust
pub use native_proc_macro::__native::{NARROW, NONE, Part, Parts, WIDE};
```

  and drop the now-unused names from its `use rustc_hash::FxHashMap;` / `use span::{..}` lines
  (`FxHashMap`; keep what `relative`/`read` still use).

> forwarded to usercode (requests, pass 22).

### 2. `proc_macro/lib.rs` `__native`: replace `pub trait Host { .. }` (lines 295–327) with

```rust
    /// What only blaze's database or lexer can answer of an expansion: asked when a macro
    /// asks a span's place, file or text, joins, parents or cuts a span, or makes tokens of
    /// text, never per token.
    pub trait Ask {
        /// The span covering both, `None` when they can't be joined.
        fn join(&mut self, first: span::Span, second: span::Span) -> Option<span::Span>;
        fn parent(&mut self, span: span::Span) -> Option<span::Span>;
        fn source(&mut self, span: span::Span) -> Option<span::Span>;
        /// The part of `span` between the module's encoded bounds, `None` when outside it.
        fn subspan(&mut self, span: span::Span, start: u64, end: u64) -> Option<span::Span>;
        fn byte_range(&mut self, span: span::Span) -> [u32; 2];
        fn line_column(&mut self, span: span::Span) -> Option<(u32, u32)>;
        fn file(&mut self, span: span::Span) -> Option<String>;
        fn local_file(&mut self, span: span::Span) -> Option<String>;
        fn source_text(&mut self, span: span::Span) -> Option<String>;
        /// The stream of `text` at `call_site` as the columns [`wire::Reader`] reads, its
        /// spans given words in `parts`, or why it is none.
        fn ts_from_str(&mut self, parts: &mut Parts, text: &str, call_site: span::Span)
            -> Result<Vec<u8>, String>;
        /// The literal of `text` as the columns of a stream of it alone.
        fn literal_from_str(&mut self, parts: &mut Parts, text: &str, call_site: span::Span)
            -> Option<Vec<u8>>;
    }

    /// The host of one expansion: its span parts and the words of its def, call and mixed
    /// sites, answering every span function a macro calls per token itself.
    pub struct Host<'a> {
        pub parts: &'a mut Parts,
        pub sites: [u64; 3],
        // dyn: blaze-expand's database and lexer, which this crate can't name (blaze-expand
        // depends on it); asked only off the per-token path (`Ask`).
        pub ask: &'a mut dyn Ask,
    }

    impl Host<'_> {
        fn span(&self, word: u64) -> Option<span::Span> {
            self.parts.get(word)
        }

        fn call_site_span(&self) -> span::Span {
            self.span(self.sites[1]).expect("the current call site is one of the expansion's")
        }

        fn def_site(&mut self) -> u64 {
            self.sites[0]
        }

        fn call_site(&mut self) -> u64 {
            self.sites[1]
        }

        fn mixed_site(&mut self) -> u64 {
            self.sites[2]
        }

        fn span_join(&mut self, a: u64, b: u64) -> u64 {
            let (Some(first), Some(second)) = (self.span(a), self.span(b)) else { return NONE };
            self.ask.join(first, second).map_or(NONE, |it| self.parts.pack(it))
        }

        fn span_resolved_at(&mut self, a: u64, at: u64) -> u64 {
            let (Some(span), Some(at)) = (self.span(a), self.span(at)) else { return a };
            self.parts.pack(span::Span { ctx: at.ctx, ..span })
        }

        fn span_located_at(&mut self, a: u64, b: u64) -> u64 {
            let (Some(span), Some(other)) = (self.span(a), self.span(b)) else { return b };
            self.parts.pack(span::Span { ctx: span.ctx, ..other })
        }

        fn span_parent(&mut self, a: u64) -> u64 {
            let Some(span) = self.span(a) else { return NONE };
            self.ask.parent(span).map_or(NONE, |it| self.parts.pack(it))
        }

        fn span_source(&mut self, a: u64) -> u64 {
            let Some(span) = self.span(a) else { return a };
            self.ask.source(span).map_or(a, |it| self.parts.pack(it))
        }

        fn span_start(&mut self, a: u64) -> u64 {
            let Some(span) = self.span(a) else { return a };
            if span.anchor.ast_id == span::FIXUP_ERASED_FILE_AST_ID_MARKER {
                return a;
            }
            self.parts.pack(span::Span { range: TextRange::empty(span.range.start()), ..span })
        }

        fn span_end(&mut self, a: u64) -> u64 {
            let Some(span) = self.span(a) else { return a };
            if span.anchor.ast_id == span::FIXUP_ERASED_FILE_AST_ID_MARKER {
                return a;
            }
            self.parts.pack(span::Span { range: TextRange::empty(span.range.end()), ..span })
        }

        fn span_subspan(&mut self, a: u64, start: u64, end: u64) -> u64 {
            let Some(span) = self.span(a) else { return NONE };
            self.ask.subspan(span, start, end).map_or(NONE, |it| self.parts.pack(it))
        }

        fn span_pack(&mut self, start: u32, len: u32, part: u32) -> u64 {
            self.parts.pack_fields(start, len, part)
        }

        fn span_unpack(&mut self, a: u64) -> [u32; 3] {
            self.parts.fields(a)
        }

        fn span_byte_range(&mut self, a: u64) -> [u32; 2] {
            let Some(span) = self.span(a) else { return [0, 0] };
            self.ask.byte_range(span)
        }

        fn span_line(&mut self, a: u64) -> u32 {
            let span = self.span(a);
            span.and_then(|span| self.ask.line_column(span)).map_or(1, |(line, _)| line)
        }

        fn span_column(&mut self, a: u64) -> u32 {
            let span = self.span(a);
            span.and_then(|span| self.ask.line_column(span)).map_or(1, |(_, column)| column)
        }

        fn span_debug(&mut self, a: u64) -> Option<String> {
            // alloc: the text the macro asked for.
            self.span(a).map(|it| format!("{it:?}"))
        }

        fn span_file(&mut self, a: u64) -> Option<String> {
            let span = self.span(a)?;
            self.ask.file(span)
        }

        fn span_local_file(&mut self, a: u64) -> Option<String> {
            let span = self.span(a)?;
            self.ask.local_file(span)
        }

        fn span_source_text(&mut self, a: u64) -> Option<String> {
            let span = self.span(a)?;
            self.ask.source_text(span)
        }

        fn ts_from_str(&mut self, text: &str) -> Result<Vec<u8>, String> {
            let call_site = self.call_site_span();
            self.ask.ts_from_str(self.parts, text, call_site)
        }

        fn literal_from_str(&mut self, text: &str) -> Option<Vec<u8>> {
            let call_site = self.call_site_span();
            self.ask.literal_from_str(self.parts, text, call_site)
        }

        #[inline]
        fn pack(&mut self, span: span::Span) -> u64 {
            self.parts.pack(span)
        }

        #[inline]
        fn get(&self, word: u64) -> Option<span::Span> {
            self.parts.get(word)
        }
    }
```

(These are `Host`'s answers as `native/host.rs` gives them today, line for line, with the
database questions moved behind `ask`.)

> forwarded to usercode (requests, pass 22).

### 3. The thread-local and its two functions

- `static HOST: Cell<Option<*mut (dyn Host + 'static)>>` and its `// dyn:` line become
  `static HOST: Cell<Option<*mut Host<'static>>> = const { Cell::new(None) };`
- `with_host`: drop its three `// dyn:` lines; signature
  `pub fn with_host<R>(host: &mut Host<'_>, f: impl FnOnce() -> R) -> R`;
  `struct Restore(Option<*mut Host<'static>>);`; and in place of the `let host: *mut (dyn Host + '_)`
  and `transmute` lines:

```rust
        // SAFETY: only the lifetime changes. The pointer is read by `host` alone, on this
        // thread, while `f` runs: `Restore` puts the one before it back when `f` returns or
        // unwinds, before the borrow it came from ends.
        let host = std::ptr::from_mut(host).cast::<Host<'static>>();
```

  (the `SAFETY` text is today's; `cast` is not `unsafe`, so the comment sits on the deref in
  `host` below, where it already is; drop it here if the lint flags a `SAFETY` with no `unsafe`).
- `host`: drop its two `// dyn:` lines; signature
  `pub(crate) fn host<R>(f: impl FnOnce(&mut Host<'_>) -> R) -> R`;
  `struct Restore(*mut Host<'static>);`; the last line becomes
  `f(unsafe { &mut *host.cast::<Host<'_>>() })` with today's `SAFETY` comment, plus at its end:
  "`f` is given the host at a lifetime of its own, never `'static`."
- `unpacked(host: &mut dyn Host, ..)` → `unpacked(host: &mut Host<'_>, ..)`, and `push`'s
  `host: &mut dyn Host` → `host: &mut Host<'_>`; drop both `// dyn:` lines. `registered`,
  `word`, `built` and every `sys` function are unchanged (they call the same method names, now
  inherent).

> forwarded to usercode (requests, pass 22).

### 4. `crates/blaze-expand/src/native/host.rs`

- `pub(super) struct Host<'a> { db, parts, sites }` becomes

```rust
/// What only the database and the lexer answer of one expansion (`__native::Ask`).
pub(super) struct Ask<'a> {
    /// What is asked of a span, or `None` where nothing can be, as in a test.
    pub(super) db: Option<&'a Db>,
}
```

  with `fn ask(&mut self, req)` and `fn line_column(&mut self, span)` kept as inherent
  methods (`line_column` unchanged); drop `span`, `number`, `call_site_span`; `written`
  becomes a free function
  `fn written(parts: &mut Parts, trees: impl IntoIterator<Item = Tree>, call_site: Span) -> Vec<u8>`
  with the body `wire::relative(&wire::invisible(trees, call_site), parts, &mut out)`.
- `impl native_proc_macro::__native::Host for Host<'_>` becomes
  `impl native_proc_macro::__native::Ask for Ask<'_>` with exactly:

```rust
    fn join(&mut self, first: Span, second: Span) -> Option<Span> {
        spans::join(first, second, |req| self.ask(req))
    }

    fn parent(&mut self, span: Span) -> Option<Span> {
        let (file_id, ast_id, start, end) = located(span);
        let req = SubRequest::SpanParent { file_id, ast_id, start, end, ctx: span.ctx.into_u32() };
        match self.ask(req) {
            Some(SubResponse::SpanParentResult { parent_span: Some(it) }) => {
                Some(spanned(it.file_id, it.ast_id, it.start, it.end, it.ctx))
            }
            _ => None,
        }
    }

    fn source(&mut self, span: Span) -> Option<Span> {
        let (file_id, ast_id, start, end) = located(span);
        let req = SubRequest::SpanSource { file_id, ast_id, start, end, ctx: span.ctx.into_u32() };
        match self.ask(req) {
            Some(SubResponse::SpanSourceResult { file_id, ast_id, start, end, ctx }) => {
                Some(spanned(file_id, ast_id, start, end, ctx))
            }
            _ => None,
        }
    }

    fn subspan(&mut self, span: Span, start: u64, end: u64) -> Option<Span> {
        spans::subspan(span, start, end)
    }

    fn byte_range(&mut self, span: Span) -> [u32; 2] {
        let (file_id, ast_id, start, end) = located(span);
        let range = match self.ask(SubRequest::ByteRange { file_id, ast_id, start, end }) {
            Some(SubResponse::ByteRangeResult { range }) => range,
            _ => usize::from(span.range.start())..usize::from(span.range.end()),
        };
        [range.start as u32, range.end as u32]
    }

    fn line_column(&mut self, span: Span) -> Option<(u32, u32)> {
        Ask::line_column(self, span)
    }

    fn file(&mut self, span: Span) -> Option<String> {
        let req = SubRequest::FilePath { file_id: span.anchor.file_id.file_id().index() };
        match self.ask(req) {
            Some(SubResponse::FilePathResult { name }) => Some(name),
            // alloc: none, an empty name.
            _ => Some(String::new()),
        }
    }

    fn local_file(&mut self, span: Span) -> Option<String> {
        let req = SubRequest::LocalFilePath { file_id: span.anchor.file_id.file_id().index() };
        match self.ask(req) {
            Some(SubResponse::LocalFilePathResult { name }) => name,
            _ => None,
        }
    }

    fn source_text(&mut self, span: Span) -> Option<String> {
        let (file_id, ast_id, start, end) = located(span);
        match self.ask(SubRequest::SourceText { file_id, ast_id, start, end }) {
            Some(SubResponse::SourceTextResult { text }) => text,
            _ => None,
        }
    }

    fn ts_from_str(&mut self, parts: &mut Parts, text: &str, call_site: Span) -> Result<Vec<u8>, String> {
        match lex::stream(text, call_site) {
            Ok(trees) => Ok(written(parts, trees, call_site)),
            // alloc: the message of text refused.
            Err(e) => Err(format!("failed to parse str to token stream: {e}")),
        }
    }

    fn literal_from_str(&mut self, parts: &mut Parts, text: &str, call_site: Span) -> Option<Vec<u8>> {
        let literal = lex::literal_from_str(text, call_site)?;
        Some(written(parts, [Tree::Leaf(tt::Leaf::Literal(literal))], call_site))
    }
```

  (`span_parent` returned `NONE`, `span_source` `a`, on no answer: `Host` above keeps that by
  `map_or`.) Rename the inherent `line_column` to avoid the clash if the compiler asks
  (e.g. `asked_line_column`). Drop the now-unused `FIXUP_ERASED_FILE_AST_ID_MARKER`/`TextRange`
  imports and `NONE`.

> forwarded to usercode (requests, pass 22).

### 5. Callers

- `native/mod.rs` `expand`: `let mut host = Host { db, parts: &mut parts, sites };` becomes

```rust
    let mut ask = host::Ask { db };
    let mut host = native_proc_macro::__native::Host { parts: &mut parts, sites, ask: &mut ask };
```

  and `use self::host::Host;` goes.
- `native/tests.rs:51`: `let mut host = host::Host { db: None, parts: &mut parts, sites: words };`
  becomes the same two lines with `db: None` and `sites: words`.

Tests: the existing `native/tests.rs` differential tests (bridge output equal to the wasm
module's, span for span) cover it unchanged; nothing new to pin.

> forwarded to usercode (requests, pass 22).
