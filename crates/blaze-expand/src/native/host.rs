//! What the bridge's `proc_macro` is answered (`proc_macro/lib.rs`, `__native::Host`):
//! what the wasm host answers a module (`crates/blaze-macro-wasm/src/lib.rs`, `add_host`), the
//! same word for the same span and the same question of the database for the same call, so
//! that a macro expands to the same tokens either way.

use base_db::Db;
use load_cargo::{SubRequest, SubResponse};
use span::{
    EditionedFileId, ErasedFileAstId, FIXUP_ERASED_FILE_AST_ID_MARKER, Span, SpanAnchor,
    SyntaxContext, TextRange, TextSize,
};

use super::{
    lex,
    wire::{self, NONE, Parts, Tree},
};

/// The host of one expansion.
pub(super) struct Host<'a> {
    /// What is asked of a span, or `None` where nothing can be, as in a test.
    pub(super) db: Option<&'a Db>,
    pub(super) parts: &'a mut Parts,
    /// The words of the def, call and mixed sites.
    pub(super) sites: [u64; 3],
}

impl Host<'_> {
    fn ask(&mut self, req: SubRequest) -> Option<SubResponse> {
        Some(load_cargo::answer(self.db?, req))
    }

    fn span(&self, word: u64) -> Option<Span> {
        self.parts.get(word)
    }

    fn number(&mut self, span: Span) -> u64 {
        self.parts.pack(span)
    }

    fn join(&mut self, first: Span, second: Span) -> Option<Span> {
        if first.anchor.ast_id == FIXUP_ERASED_FILE_AST_ID_MARKER {
            return Some(second);
        }
        if second.anchor.ast_id == FIXUP_ERASED_FILE_AST_ID_MARKER {
            return Some(first);
        }
        if first.anchor != second.anchor {
            if first.anchor.file_id != second.anchor.file_id {
                return None;
            }
            let req = SubRequest::SpanJoin {
                file_id: first.anchor.file_id.as_u32(),
                ast_id_first: first.anchor.ast_id.into_raw(),
                start_first: first.range.start().into(),
                end_first: first.range.end().into(),
                ctx_first: first.ctx.into_u32(),
                ast_id_second: second.anchor.ast_id.into_raw(),
                start_second: second.range.start().into(),
                end_second: second.range.end().into(),
                ctx_second: second.ctx.into_u32(),
            };
            let Some(SubResponse::SpanJoinResult { span: Some(it) }) = self.ask(req) else {
                return None;
            };
            let anchor = SpanAnchor {
                file_id: first.anchor.file_id,
                ast_id: ErasedFileAstId::from_raw(it.ast_id),
            };
            // SAFETY: `from_u32` asks for a value `into_u32` gave of this database's contexts:
            // `it.ctx` is what `load_cargo::answer` read from `self.db` for this join, in this
            // process. Memory safety doesn't rest on it: a wrong value panics in `from_u32` or
            // in salsa's checked lookup, or names another context's hygiene.
            let ctx = unsafe { SyntaxContext::from_u32(it.ctx) };
            return Some(Span { range: range(it.start, it.end), anchor, ctx });
        }
        if first.ctx != second.ctx {
            return Some(first);
        }
        Some(Span { range: first.range.cover(second.range), anchor: second.anchor, ctx: second.ctx })
    }

    fn line_column(&mut self, span: Span) -> Option<(u32, u32)> {
        let req = SubRequest::LineColumn {
            file_id: span.anchor.file_id.as_u32(),
            ast_id: span.anchor.ast_id.into_raw(),
            offset: span.range.start().into(),
        };
        match self.ask(req)? {
            SubResponse::LineColumnResult { line, column } => Some((line, column)),
            _ => None,
        }
    }

    fn call_site_span(&self) -> Span {
        self.span(self.sites[1]).expect("the current call site is one of the expansion's")
    }

    /// `trees` as the columns the macro reads of text it asked to be tokens of.
    fn written(&mut self, trees: &[Tree]) -> Vec<u8> {
        let mut out = Vec::new();
        wire::relative(&wire::invisible(trees, self.call_site_span()), self.parts, &mut out);
        out
    }
}

fn subspan(span: Span, start: u64, end: u64) -> Option<Span> {
    if span.anchor.ast_id == FIXUP_ERASED_FILE_AST_ID_MARKER {
        return Some(span);
    }
    let length: u32 = span.range.len().into();
    let bound = |it: u64, unbounded: u32, included_end: bool| -> Option<u32> {
        let at = u32::try_from(it / 4).ok()?;
        match (it, it % 4) {
            (0, _) => Some(unbounded),
            (_, 1) if included_end => at.checked_add(1),
            (_, 1) => Some(at),
            (_, 3) if included_end => Some(at),
            (_, 3) => at.checked_add(1),
            _ => None,
        }
    };
    let start = bound(start, 0, false)?;
    let end = bound(end, length, true)?;
    let span_start: u32 = span.range.start().into();
    if (u32::MAX - start) < span_start
        || (u32::MAX - end) < span_start
        || start >= end
        || end > length
    {
        return None;
    }
    let range = TextRange::new(TextSize::from(start), TextSize::from(end)) + span.range.start();
    Some(Span { range, ..span })
}

fn range(start: u32, end: u32) -> TextRange {
    TextRange::new(TextSize::new(start), TextSize::new(end.max(start)))
}

fn spanned(file_id: u32, ast_id: u32, start: u32, end: u32, ctx: u32) -> Span {
    let anchor = SpanAnchor {
        file_id: EditionedFileId::from_raw(file_id),
        ast_id: ErasedFileAstId::from_raw(ast_id),
    };
    // SAFETY: `from_u32` asks for a value `into_u32` gave of this database's contexts: every
    // caller passes a `ctx` that `load_cargo::answer` read from the host's database in this
    // process (`span_parent`, `span_source`). Memory safety doesn't rest on it: a wrong value
    // panics in `from_u32` or in salsa's checked lookup, or names another context's hygiene.
    Span { range: range(start, end), anchor, ctx: unsafe { SyntaxContext::from_u32(ctx) } }
}

fn located(span: Span) -> (u32, u32, u32, u32) {
    let (file_id, ast_id) = (span.anchor.file_id.as_u32(), span.anchor.ast_id.into_raw());
    (file_id, ast_id, span.range.start().into(), span.range.end().into())
}

impl native_proc_macro::__native::Host for Host<'_> {
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
        self.join(first, second).map_or(NONE, |it| self.number(it))
    }

    fn span_resolved_at(&mut self, a: u64, at: u64) -> u64 {
        let (Some(span), Some(at)) = (self.span(a), self.span(at)) else { return a };
        self.number(Span { ctx: at.ctx, ..span })
    }

    fn span_located_at(&mut self, a: u64, b: u64) -> u64 {
        let (Some(span), Some(other)) = (self.span(a), self.span(b)) else { return b };
        self.number(Span { ctx: span.ctx, ..other })
    }

    fn span_parent(&mut self, a: u64) -> u64 {
        let Some(span) = self.span(a) else { return NONE };
        let (file_id, ast_id, start, end) = located(span);
        let req = SubRequest::SpanParent { file_id, ast_id, start, end, ctx: span.ctx.into_u32() };
        match self.ask(req) {
            Some(SubResponse::SpanParentResult { parent_span: Some(it) }) => {
                self.number(spanned(it.file_id, it.ast_id, it.start, it.end, it.ctx))
            }
            _ => NONE,
        }
    }

    fn span_source(&mut self, a: u64) -> u64 {
        let Some(span) = self.span(a) else { return a };
        let (file_id, ast_id, start, end) = located(span);
        let req = SubRequest::SpanSource { file_id, ast_id, start, end, ctx: span.ctx.into_u32() };
        match self.ask(req) {
            Some(SubResponse::SpanSourceResult { file_id, ast_id, start, end, ctx }) => {
                self.number(spanned(file_id, ast_id, start, end, ctx))
            }
            _ => a,
        }
    }

    fn span_start(&mut self, a: u64) -> u64 {
        let Some(span) = self.span(a) else { return a };
        if span.anchor.ast_id == FIXUP_ERASED_FILE_AST_ID_MARKER {
            return a;
        }
        self.number(Span { range: TextRange::empty(span.range.start()), ..span })
    }

    fn span_end(&mut self, a: u64) -> u64 {
        let Some(span) = self.span(a) else { return a };
        if span.anchor.ast_id == FIXUP_ERASED_FILE_AST_ID_MARKER {
            return a;
        }
        self.number(Span { range: TextRange::empty(span.range.end()), ..span })
    }

    fn span_subspan(&mut self, a: u64, start: u64, end: u64) -> u64 {
        let Some(span) = self.span(a) else { return NONE };
        subspan(span, start, end).map_or(NONE, |it| self.number(it))
    }

    fn span_pack(&mut self, start: u32, len: u32, part: u32) -> u64 {
        self.parts.pack_fields(start, len, part)
    }

    fn span_unpack(&mut self, a: u64) -> [u32; 3] {
        self.parts.fields(a)
    }

    fn span_byte_range(&mut self, a: u64) -> [u32; 2] {
        let Some(span) = self.span(a) else { return [0, 0] };
        let (file_id, ast_id, start, end) = located(span);
        let range = match self.ask(SubRequest::ByteRange { file_id, ast_id, start, end }) {
            Some(SubResponse::ByteRangeResult { range }) => range,
            _ => usize::from(span.range.start())..usize::from(span.range.end()),
        };
        [range.start as u32, range.end as u32]
    }

    fn span_line(&mut self, a: u64) -> u32 {
        let span = self.span(a);
        span.and_then(|span| self.line_column(span)).map_or(1, |(line, _)| line)
    }

    fn span_column(&mut self, a: u64) -> u32 {
        let span = self.span(a);
        span.and_then(|span| self.line_column(span)).map_or(1, |(_, column)| column)
    }

    fn span_debug(&mut self, a: u64) -> Option<String> {
        self.span(a).map(|it| format!("{it:?}"))
    }

    fn span_file(&mut self, a: u64) -> Option<String> {
        let span = self.span(a)?;
        let req = SubRequest::FilePath { file_id: span.anchor.file_id.file_id().index() };
        match self.ask(req) {
            Some(SubResponse::FilePathResult { name }) => Some(name),
            _ => Some(String::new()),
        }
    }

    fn span_local_file(&mut self, a: u64) -> Option<String> {
        let span = self.span(a)?;
        let req = SubRequest::LocalFilePath { file_id: span.anchor.file_id.file_id().index() };
        match self.ask(req) {
            Some(SubResponse::LocalFilePathResult { name }) => name,
            _ => None,
        }
    }

    fn span_source_text(&mut self, a: u64) -> Option<String> {
        let span = self.span(a)?;
        let (file_id, ast_id, start, end) = located(span);
        match self.ask(SubRequest::SourceText { file_id, ast_id, start, end }) {
            Some(SubResponse::SourceTextResult { text }) => text,
            _ => None,
        }
    }

    fn ts_from_str(&mut self, text: &str) -> Result<Vec<u8>, String> {
        match lex::stream(text, self.call_site_span()) {
            Ok(trees) => Ok(self.written(&trees)),
            Err(e) => Err(format!("failed to parse str to token stream: {e}")),
        }
    }

    fn literal_from_str(&mut self, text: &str) -> Option<Vec<u8>> {
        let literal = lex::literal_from_str(text, self.call_site_span())?;
        Some(self.written(&[Tree::Leaf(tt::Leaf::Literal(literal))]))
    }

    fn pack(&mut self, span: Span) -> u64 {
        self.parts.pack(span)
    }

    fn get(&self, word: u64) -> Option<Span> {
        self.parts.get(word)
    }
}
