//! The span operations a macro asks of its host that both hosts answer the same way:
//! `Span::join` and `Span::subspan`, as rust-analyzer's proc-macro server
//! (`proc-macro-srv/src/server_impl/rust_analyzer_span.rs`) answers them.

use load_cargo::{SubRequest, SubResponse};
use span::{
    ErasedFileAstId, FIXUP_ERASED_FILE_AST_ID_MARKER, Span, SpanAnchor, SyntaxContext, TextRange,
    TextSize,
};

/// The part of `span` between `start` and `end`, each a `Bound` as the module encodes one
/// (`0` unbounded, `4 * n + 1` included, `4 * n + 3` excluded): `None` when it is not within
/// `span`.
pub fn subspan(span: Span, start: u64, end: u64) -> Option<Span> {
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

/// The span covering `first` and `second`: `None` when they are of different files, the
/// database's join (asked by `ask`) when of different items of one.
pub fn join(
    first: Span,
    second: Span,
    ask: impl FnOnce(SubRequest) -> Option<SubResponse>,
) -> Option<Span> {
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
        let Some(SubResponse::SpanJoinResult { span: Some(it) }) = ask(req) else {
            return None;
        };
        let anchor = SpanAnchor {
            file_id: first.anchor.file_id,
            ast_id: ErasedFileAstId::from_raw(it.ast_id),
        };
        // SAFETY: `from_u32` asks for a value `into_u32` gave of this database's contexts:
        // `it.ctx` is what `ask` answered for this join, which each host gets from
        // `load_cargo::answer` of the running expansion's database, in this process. Memory
        // safety doesn't rest on it: a wrong value panics in `from_u32` or in salsa's checked
        // lookup, or names another context's hygiene.
        let ctx = unsafe { SyntaxContext::from_u32(it.ctx) };
        let range = TextRange::new(TextSize::new(it.start), TextSize::new(it.end.max(it.start)));
        return Some(Span { range, anchor, ctx });
    }
    if first.ctx != second.ctx {
        return Some(first);
    }
    Some(Span { range: first.range.cover(second.range), anchor: second.anchor, ctx: second.ctx })
}

#[cfg(test)]
mod tests {
    use super::{super::at, *};

    #[test]
    fn a_subspan_is_of_the_span_and_within_it() {
        let it = at(10, 20);
        let range = |start: u32, end: u32| TextRange::new(start.into(), end.into());
        // Unbounded, included and excluded bounds as the module writes them.
        assert_eq!(subspan(it, 0, 0).unwrap().range, range(10, 20));
        assert_eq!(subspan(it, 1 + 4 * 2, 3 + 4 * 5).unwrap().range, range(12, 15));
        assert_eq!(subspan(it, 3 + 4 * 2, 1 + 4 * 5).unwrap().range, range(13, 16));
        assert!(subspan(it, 1 + 4 * 5, 3 + 4 * 5).is_none());
        assert!(subspan(it, 0, 3 + 4 * 11).is_none());
    }
}
