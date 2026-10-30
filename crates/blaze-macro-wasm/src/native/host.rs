//! What the bridge's `proc_macro` asks blaze's database and lexer (`proc_macro/lib.rs`,
//! `__native::Ask`), whose `__native::Host` answers every other call itself: what the wasm
//! host answers a module (`crates/blaze-macro-wasm/src/lib.rs`, `add_host`), the
//! same word for the same span and the same question of the database for the same call, so
//! that a macro expands to the same tokens either way.

use base_db::Db;
use blaze_lower::{SubRequest, SubResponse};
use span::Span;

use proc_macro_api::bridge::{
    lex,
    spans::{self, spanned},
    wire::{self, Parts, Tree},
};

/// What only the database and the lexer answer of one expansion (`__native::Ask`).
pub(super) struct Ask<'a> {
    /// What is asked of a span, or `None` where nothing can be, as in a test.
    pub(super) db: Option<&'a Db>,
}

impl Ask<'_> {
    fn ask(&mut self, req: SubRequest) -> Option<SubResponse> {
        Some(blaze_lower::answer(self.db?, req))
    }
}

/// `trees` as the columns the macro reads of text it asked to be tokens of.
fn written(parts: &mut Parts, trees: impl IntoIterator<Item = Tree>, call_site: Span) -> Vec<u8> {
    // alloc: the bytes of text the macro asked to be tokens of, which it is given.
    let mut out = Vec::new();
    wire::relative(&wire::invisible(trees, call_site), parts, &mut out);
    out
}

fn located(span: Span) -> (u32, u32, u32, u32) {
    let (file_id, ast_id) = (span.anchor.file_id.as_u32(), span.anchor.ast_id.into_raw());
    (file_id, ast_id, span.range.start().into(), span.range.end().into())
}

impl native_proc_macro::__native::Ask for Ask<'_> {
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
}
