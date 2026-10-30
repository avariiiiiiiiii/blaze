//! What a proc macro asks of the database while it expands (a span's text, file, line, bytes,
//! source or parent), answered as rustc's proc-macro server answers it: the one place that
//! answers it, whichever server or runtime asked.

#![forbid(unsafe_code)]

use base_db::{Db, FileId};
use proc_macro_api::bridge::questions::{ParentSpan, SpanJoin};
use span::{ROOT_ERASED_FILE_AST_ID, Span, SpanAnchor, SyntaxContext, TextRange, TextSize};

pub use proc_macro_api::bridge::questions::{SubRequest, SubResponse};

/// `req` answered from `db`, the database of the expansion that asked it.
pub fn answer(db: &Db, req: SubRequest) -> SubResponse {
    match req {
        SubRequest::LocalFilePath { file_id } => {
            SubResponse::LocalFilePathResult { name: path_of(db, FileId::from_raw(file_id)) }
        }
        SubRequest::FilePath { file_id } => {
            SubResponse::FilePathResult { name: path_of(db, FileId::from_raw(file_id)).unwrap_or_default() }
        }
        // Not incremental: requires full file text.
        SubRequest::SourceText { file_id, ast_id, start, end } => {
            let range = resolve(db, file_id, ast_id, TextRange::new(TextSize::from(start), TextSize::from(end)));
            let source = db.file_text(range.file_id.file_id(db)).text(db);
            let text = source.get(usize::from(range.range.start())..usize::from(range.range.end())).map(ToOwned::to_owned);
            SubResponse::SourceTextResult { text }
        }
        // Not incremental: requires building line index.
        SubRequest::LineColumn { file_id, ast_id, offset } => {
            let range = resolve(db, file_id, ast_id, TextRange::empty(TextSize::from(offset)));
            // proc_macro::Span line/column are 1-based
            let (line, column) = db
                .line_column(range.file_id.file_id(db), range.range.start())
                .map(|(line, col)| (line + 1, col + 1))
                .unwrap_or((1, 1));
            SubResponse::LineColumnResult { line, column }
        }
        // Not incremental: requires global span resolution.
        SubRequest::ByteRange { file_id, ast_id, start, end } => {
            let range = resolve(db, file_id, ast_id, TextRange::new(TextSize::from(start), TextSize::from(end)));
            SubResponse::ByteRangeResult { range: range.range.into() }
        }
        SubRequest::SpanSource { file_id, ast_id, start, end, ctx } => {
            let mut current_span = span_of(file_id, ast_id, start, end, ctx);
            let mut current_ctx = current_span.ctx;
            while let Some(macro_call_id) = current_ctx.outer_expn(db) {
                let macro_call_loc = hir_expand::MacroCallId::from(macro_call_id).loc(db);
                let call_site_file = macro_call_loc.kind.file_id();
                let resolved = hir_expand::resolve_span(db, current_span);
                current_ctx = macro_call_loc.ctxt;
                current_span = Span {
                    range: resolved.range,
                    anchor: SpanAnchor { file_id: resolved.file_id.span_file_id(db), ast_id: ROOT_ERASED_FILE_AST_ID },
                    ctx: current_ctx,
                };
                if call_site_file.file_id().is_some() {
                    break;
                }
            }
            let resolved = hir_expand::resolve_span(db, current_span);
            SubResponse::SpanSourceResult {
                file_id: resolved.file_id.span_file_id(db).as_u32(),
                ast_id: ROOT_ERASED_FILE_AST_ID.into_raw(),
                start: u32::from(resolved.range.start()),
                end: u32::from(resolved.range.end()),
                ctx: current_span.ctx.into_u32(),
            }
        }
        SubRequest::SpanParent { file_id, ast_id, start, end, ctx } => {
            let span = span_of(file_id, ast_id, start, end, ctx);
            let parent_span = span.ctx.outer_expn(db).and_then(|macro_call_id| {
                let macro_call_loc = hir_expand::MacroCallId::from(macro_call_id).loc(db);
                let editioned_file_id = macro_call_loc.kind.file_id().file_id()?;
                let range = hir_expand::HirFileId::from(editioned_file_id)
                    .ast_id_map(db)
                    .get_erased(macro_call_loc.kind.erased_ast_id())
                    .text_range();
                Some(ParentSpan {
                    file_id: editioned_file_id.span_file_id(db).as_u32(),
                    ast_id: ROOT_ERASED_FILE_AST_ID.into_raw(),
                    start: u32::from(range.start()),
                    end: u32::from(range.end()),
                    ctx: macro_call_loc.ctxt.into_u32(),
                })
            });
            SubResponse::SpanParentResult { parent_span }
        }
        // Two spans of one file, of different items (the caller joins spans of one anchor
        // itself): the span from where the first starts to where the second ends, both
        // resolved to the file, as rustc's `Span::join` gives it. Spans of other contexts join
        // to the first, as they do under one anchor.
        SubRequest::SpanJoin {
            file_id,
            ast_id_first,
            start_first,
            end_first,
            ctx_first,
            ast_id_second,
            start_second,
            end_second,
            ctx_second,
        } => {
            let first = resolve(db, file_id, ast_id_first, TextRange::new(start_first.into(), end_first.into()));
            let range = match ctx_first == ctx_second {
                true => {
                    let second =
                        resolve(db, file_id, ast_id_second, TextRange::new(start_second.into(), end_second.into()));
                    first.range.cover(second.range)
                }
                false => first.range,
            };
            SubResponse::SpanJoinResult {
                span: Some(SpanJoin {
                    ast_id: ROOT_ERASED_FILE_AST_ID.into_raw(),
                    start: u32::from(range.start()),
                    end: u32::from(range.end()),
                    ctx: ctx_first,
                }),
            }
        }
    }
}

/// The absolute path of `file`, if it has one.
fn path_of(db: &Db, file: FileId) -> Option<String> {
    let source_root_id = db.file_source_root(file).source_root_id(db);
    let source_root = db.source_root(source_root_id).source_root(db);
    source_root.path_for_file(&file).and_then(|path| path.as_path()).map(|path| path.to_string())
}

fn span_of(file_id: u32, ast_id: u32, start: u32, end: u32, ctx: u32) -> Span {
    Span {
        range: TextRange::new(TextSize::from(start), TextSize::from(end)),
        anchor: SpanAnchor {
            file_id: span::EditionedFileId::from_raw(file_id),
            ast_id: span::ErasedFileAstId::from_raw(ast_id),
        },
        // The server echoes the contexts blaze sent it; a made-up value (a buggy or hostile
        // server) gives a panic or another context's hygiene data.
        ctx: SyntaxContext::from_u32(ctx),
    }
}

/// `range` relative to item `ast_id` of `file_id`, resolved to the file.
fn resolve(db: &Db, file_id: u32, ast_id: u32, range: TextRange) -> hir_expand::FileRange {
    let file_id = span::EditionedFileId::from_raw(file_id);
    let span = Span {
        range,
        anchor: SpanAnchor { file_id, ast_id: span::ErasedFileAstId::from_raw(ast_id) },
        ctx: SyntaxContext::root(file_id.edition()),
    };
    hir_expand::resolve_span(db, span)
}
