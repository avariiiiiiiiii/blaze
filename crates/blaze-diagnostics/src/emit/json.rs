//! rustc's `--error-format=json`: `compiler/rustc_errors/src/json.rs` (`Diagnostic`,
//! `DiagnosticSpan` and the rest, their keys in its order) at rust-lang/rust
//! ad3d0bc141a02cf446e384136d250a1f6950fed5 (MIT or Apache-2.0).

use serde::Serialize;

use super::{File, Sources, backtrace, codes, originates_in};
use crate::model::{Applicability, Child, Code, Diag, Expansion, MultiSpan, Span, Suggestion};

#[derive(Serialize)]
struct Diagnostic {
    /// Only on a diagnostic of its own, not on its children: rustc's `EmitTyped` tag.
    #[serde(rename = "$message_type", skip_serializing_if = "Option::is_none")]
    message_type: Option<&'static str>,
    message: String,
    code: Option<DiagnosticCode>,
    level: &'static str,
    spans: Vec<DiagnosticSpan>,
    children: Vec<Diagnostic>,
    rendered: Option<String>,
}

#[derive(Serialize)]
struct DiagnosticSpan {
    file_name: String,
    byte_start: u32,
    byte_end: u32,
    line_start: usize,
    line_end: usize,
    column_start: usize,
    column_end: usize,
    is_primary: bool,
    text: Vec<DiagnosticSpanLine>,
    label: Option<String>,
    suggested_replacement: Option<String>,
    suggestion_applicability: Option<&'static str>,
    expansion: Option<Box<DiagnosticSpanMacroExpansion>>,
}

#[derive(Serialize)]
struct DiagnosticSpanLine {
    text: String,
    highlight_start: usize,
    highlight_end: usize,
}

#[derive(Serialize)]
struct DiagnosticSpanMacroExpansion {
    span: DiagnosticSpan,
    macro_decl_name: String,
    /// `null` where the model knows no definition of the macro, which rustc always has.
    def_site_span: Option<DiagnosticSpan>,
}

#[derive(Serialize)]
struct DiagnosticCode {
    code: String,
    explanation: Option<&'static str>,
}

/// `diag` as rustc's JSON line, with `rendered` as its text: `from_errors_diagnostic`. Its
/// children are the ones it was given, without the note on the macro it originates in that
/// rustc's text emitter adds, then a `help` for each suggestion.
pub(super) fn line(diag: &Diag, sources: &mut Sources, rendered: String) -> String {
    let added = originates_in(diag);
    let mut children: Vec<Diagnostic> = diag
        .children
        .iter()
        .filter(|it| !(it.span.is_empty() && Some(&it.message) == added.as_ref()))
        .map(|it| child(it, sources))
        .collect();
    if !diag.suggestions_disabled {
        children.extend(diag.suggestions.iter().map(|it| suggestion(it, sources)));
    }
    let code = diag.code.as_ref().map(|code| match code {
        Code::Error(code) => {
            DiagnosticCode { code: code.clone(), explanation: codes::explanation(code) }
        }
        Code::Lint(name) => DiagnosticCode { code: name.clone(), explanation: None },
    });
    let diagnostic = Diagnostic {
        message_type: Some("diagnostic"),
        message: diag.message.clone(),
        code,
        level: diag.level.name(),
        spans: spans(&diag.span, sources),
        children,
        rendered: Some(rendered),
    };
    serde_json::to_string(&diagnostic).unwrap_or_else(|error| {
        panic!("blaze-diagnostics: `{}` as JSON: {error}", diag.message)
    })
}

fn child(child: &Child, sources: &mut Sources) -> Diagnostic {
    Diagnostic {
        message_type: None,
        message: child.message.clone(),
        code: None,
        level: child.level.name(),
        spans: spans(&child.span, sources),
        children: Vec::new(),
        rendered: None,
    }
}

/// rustc's `DiagnosticSpan::from_suggestion`: each part of each way to substitute, as a
/// primary span carrying its replacement.
fn suggestion(suggestion: &Suggestion, sources: &mut Sources) -> Diagnostic {
    let spans = suggestion
        .substitutions
        .iter()
        .flatten()
        .map(|(span, replacement)| {
            let backtrace = backtrace(span);
            let suggested = Some((replacement.as_str(), suggestion.applicability));
            self::span(span, true, None, suggested, &backtrace, sources)
        })
        .collect();
    Diagnostic {
        message_type: None,
        message: suggestion.message.clone(),
        code: None,
        level: "help",
        spans,
        children: Vec::new(),
        rendered: None,
    }
}

fn spans(span: &MultiSpan, sources: &mut Sources) -> Vec<DiagnosticSpan> {
    span.span_labels()
        .into_iter()
        .map(|it| {
            let backtrace = backtrace(it.span);
            self::span(it.span, it.is_primary, it.label.map(str::to_owned), None, &backtrace, sources)
        })
        .collect()
}

/// rustc's `DiagnosticSpan::from_span_full`: `span` placed, with the expansions it is in from
/// `backtrace` on, each call placed with the ones after it.
fn span(
    span: &Span,
    is_primary: bool,
    label: Option<String>,
    suggestion: Option<(&str, Applicability)>,
    backtrace: &[&Expansion],
    sources: &mut Sources,
) -> DiagnosticSpan {
    let file = sources.file_of(span);
    let (lo, mut hi) = (file.norm(span.lo), file.norm(span.hi));
    let mut byte_end = span.hi;
    let (line_start, column_start) = file.position(lo);
    // A suggestion that removes from the start of a line to its end removes the line.
    if column_start == 0
        && suggestion.is_some_and(|(replacement, _)| replacement.is_empty())
        && file.text[hi..].starts_with('\n')
    {
        hi += 1;
        byte_end = file.original(hi);
    }
    let (line_end, column_end) = file.position(hi);
    let expansion = backtrace.split_first().map(|(expansion, outer)| {
        let def_site_span = expansion.def_site.as_ref().map(|def| {
            let head = guess_head_span(sources, def);
            self::span(&head, false, None, None, &[], sources)
        });
        Box::new(DiagnosticSpanMacroExpansion {
            span: self::span(&expansion.call_site, false, None, None, outer, sources),
            macro_decl_name: expansion.decl_name(),
            def_site_span,
        })
    });
    DiagnosticSpan {
        file_name: span.file.clone(),
        byte_start: span.lo,
        byte_end,
        line_start,
        line_end,
        column_start: column_start + 1,
        column_end: column_end + 1,
        is_primary,
        text: lines(&file, lo, hi),
        label,
        suggested_replacement: suggestion.map(|(replacement, _)| replacement.to_owned()),
        suggestion_applicability: suggestion.map(|(_, applicability)| applicability.name()),
        expansion,
    }
}

/// rustc's `DiagnosticSpanLine::from_span` (`span_to_lines`): each line `lo..hi` is on, with
/// the columns of it on that line, 1-based in chars.
fn lines(file: &File, lo: usize, hi: usize) -> Vec<DiagnosticSpanLine> {
    if lo > hi {
        return Vec::new();
    }
    let ((lo_line, lo_column), (hi_line, hi_column)) = (file.position(lo), file.position(hi));
    let mut start = lo_column;
    let mut lines = Vec::new();
    for line in lo_line..hi_line {
        let text = file.line_text(line - 1);
        let end = text.chars().count();
        lines.push(DiagnosticSpanLine {
            text: text.to_owned(),
            highlight_start: start + 1,
            highlight_end: end + 1,
        });
        start = 0;
    }
    lines.push(DiagnosticSpanLine {
        text: file.line_text(hi_line - 1).to_owned(),
        highlight_start: start + 1,
        highlight_end: hi_column + 1,
    });
    lines
}

/// rustc's `guess_head_span` (`span_until_char(sp, '{')`): a definition up to its `{`.
fn guess_head_span(sources: &mut Sources, span: &Span) -> Span {
    let Some(snippet) = sources.snippet(span) else { return span.clone() };
    let head = snippet.split('{').next().unwrap_or("").trim_end();
    match !head.is_empty() && !head.contains('\n') {
        true => span.with_hi(span.lo + head.len() as u32),
        false => span.clone(),
    }
}
