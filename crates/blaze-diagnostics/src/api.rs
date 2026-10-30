//! A diagnostic as the agent API gives it ([`blaze_api::Diagnostic`]): rustc's JSON shape
//! (`rustc_errors/src/json.rs`, MIT or Apache-2.0), every span labelled and placed, each
//! suggestion a `help` child whose spans carry their replacement, with the facts it read and
//! its text as rustc prints it.

use blaze_api::{Diagnostic, FactRead, UnitOf};

use crate::{
    emit::{self, Sources, Style, codes},
    model::{Applicability, Child, Diag, Level, MultiSpan, Sublevel, Suggestion},
};

/// `diag` for the agent API, its `rendered` as rustc writes it in `style`, of `unit` when it is
/// a cargo unit's. Its `explanation` is rustc's for its code, as rustc's JSON gives it, in
/// every format: only a JSON writer prints it.
pub fn diagnostic(diag: &Diag, sources: &Sources, style: Style, unit: Option<UnitOf>) -> Diagnostic {
    let mut children: Vec<Diagnostic> =
        // alloc: the API's owned copy of a diagnostic written, the edge
        diag.children.iter().map(|child| self::child(child, sources)).collect();
    if !diag.suggestions_disabled {
        children.extend(diag.suggestions.iter().map(|it| suggestion(it, sources)));
    }
    Diagnostic {
        level: level(diag.level),
        // alloc: the API's owned code, of a diagnostic written
        code: diag.code.as_ref().map(|code| code.as_str().to_owned()),
        // clone: the API's owned message, of a diagnostic written
        message: diag.message.clone(),
        spans: spans(&diag.span, sources),
        children,
        facts: diag
            .facts
            .iter()
            // clone: the API's owned text of each fact, of a diagnostic written
            .map(|read| FactRead { key: read.fact.key(), what: read.what.clone() })
            .collect(), // alloc: the API's facts, of a diagnostic written
        rendered: emit::written(diag, sources, style),
        unit,
        // alloc: the API's owned explanation, of a diagnostic written
        explanation: diag.code.as_ref().and_then(|code| codes::explanation(code.as_str())).map(str::to_owned),
    }
}

fn level(level: Level) -> blaze_api::Level {
    match level {
        Level::Error => blaze_api::Level::Error,
        Level::Warning => blaze_api::Level::Warning,
        Level::Note => blaze_api::Level::Note,
        Level::Help => blaze_api::Level::Help,
        Level::FailureNote => blaze_api::Level::FailureNote,
    }
}

fn child(child: &Child, sources: &Sources) -> Diagnostic {
    let level = match child.level {
        Sublevel::Note => blaze_api::Level::Note,
        Sublevel::Help => blaze_api::Level::Help,
        Sublevel::Warning => blaze_api::Level::Warning,
    };
    Diagnostic {
        level,
        code: None,
        // clone: the API's owned message, of a diagnostic written
        message: child.message.clone(),
        spans: spans(&child.span, sources),
        children: Vec::new(), // alloc: empty, allocates nothing
        facts: Vec::new(),    // alloc: empty, allocates nothing
        rendered: String::new(), // alloc: empty, allocates nothing
        unit: None,
        explanation: None,
    }
}

/// rustc's JSON gives every part of every substitution of a suggestion as one span of its
/// child.
fn suggestion(suggestion: &Suggestion, sources: &Sources) -> Diagnostic {
    let spans = suggestion
        .substitutions
        .iter()
        .flatten()
        .map(|(span, replacement)| {
            let mut it = place(span, sources, true, None);
            // clone: the API's owned replacement, of a suggestion written
            it.replacement = Some(replacement.clone());
            it.applicability = Some(applicability(suggestion.applicability));
            it
        })
        .collect(); // alloc: the API's spans of a suggestion written
    Diagnostic {
        level: blaze_api::Level::Help,
        code: None,
        // clone: the API's owned message, of a suggestion written
        message: suggestion.message.clone(),
        spans,
        children: Vec::new(), // alloc: empty, allocates nothing
        facts: Vec::new(),    // alloc: empty, allocates nothing
        rendered: String::new(), // alloc: empty, allocates nothing
        unit: None,
        explanation: None,
    }
}

fn applicability(applicability: Applicability) -> blaze_api::Applicability {
    match applicability {
        Applicability::MachineApplicable => blaze_api::Applicability::MachineApplicable,
        Applicability::MaybeIncorrect => blaze_api::Applicability::MaybeIncorrect,
        Applicability::HasPlaceholders => blaze_api::Applicability::HasPlaceholders,
        Applicability::Unspecified => blaze_api::Applicability::Unspecified,
    }
}

fn spans(span: &MultiSpan, sources: &Sources) -> Vec<blaze_api::Span> {
    span.span_labels()
        .into_iter()
        // alloc: the API's owned labels, of a diagnostic written
        .map(|label| place(label.span, sources, label.is_primary, label.label.map(str::to_owned)))
        .collect() // alloc: the API's spans, of a diagnostic written
}

/// A span of a file blaze can't read is a bug of whoever made it: it points into nothing.
fn place(
    span: &crate::model::Span,
    sources: &Sources,
    primary: bool,
    label: Option<String>,
) -> blaze_api::Span {
    let at = |byte: u32| {
        sources.position(&span.file, byte).unwrap_or_else(|| {
            panic!(
                "blaze-diagnostics: a span at byte {byte} of `{}`, which has no such byte",
                crate::emit::shown(&span.file)
            )
        })
    };
    let (line, column) = at(span.lo);
    let (end_line, end_column) = at(span.hi);
    blaze_api::Span {
        // clone: the API's owned file name, of a span written
        file: crate::emit::shown(&span.file).to_owned(),
        line,
        column,
        end_line,
        end_column,
        label,
        byte_start: span.lo,
        byte_end: span.hi,
        primary,
        replacement: None,
        applicability: None,
    }
}

#[cfg(test)]
mod tests {
    use blaze_items::{Fact, ItemKey};
    use syntax::{TextRange, TextSize};

    use super::*;
    use crate::model::{Applicability, Span};

    fn span(lo: u32, hi: u32) -> Span {
        Span::new("src/main.rs", TextRange::new(TextSize::from(lo), TextSize::from(hi)))
    }

    fn sources() -> Sources {
        Sources::of([("src/main.rs".to_owned(), "fn main() {\n    let x: u32 = \"a\";\n}\n".to_owned())])
    }

    #[test]
    fn spans_are_placed_by_line_and_column_of_characters() {
        let mut diag = Diag::error(Some("E0308"), "mismatched types", span(29, 32));
        diag.span_label(span(29, 32), "expected `u32`, found `&str`");
        diag.span_label(span(23, 26), "expected due to this");
        let it = diagnostic(&diag, &sources(), Style::default(), None);
        assert_eq!(it.code.as_deref(), Some("E0308"));
        assert_eq!(it.level, blaze_api::Level::Error);
        let placed: Vec<_> = it
            .spans
            .iter()
            .map(|s| (s.line, s.column, s.end_line, s.end_column, s.primary, s.label.as_deref()))
            .collect();
        assert_eq!(placed, [
            (2, 18, 2, 21, true, Some("expected `u32`, found `&str`")),
            (2, 12, 2, 15, false, Some("expected due to this")),
        ]);
        assert!(it.rendered.starts_with("error[E0308]: mismatched types\n"), "{}", it.rendered);
        assert_eq!(it.explanation.as_deref(), Some(include_str!("emit/codes/E0308.md")));
    }

    #[test]
    fn a_suggestion_is_a_help_child_whose_spans_carry_the_replacement() {
        let mut diag = Diag::error(Some("E0308"), "mismatched types", span(29, 32));
        diag.note("a note");
        diag.suggestion(span(29, 32), "use a number", "1", Applicability::MaybeIncorrect);
        let it = diagnostic(&diag, &sources(), Style::default(), None);
        assert_eq!(it.children.len(), 2);
        assert_eq!((it.children[0].level, it.children[0].message.as_str()), (blaze_api::Level::Note, "a note"));
        assert!(it.children[0].spans.is_empty());
        let help = &it.children[1];
        assert_eq!((help.level, help.message.as_str()), (blaze_api::Level::Help, "use a number"));
        assert_eq!(help.spans[0].replacement.as_deref(), Some("1"));
        assert_eq!(help.spans[0].applicability, Some(blaze_api::Applicability::MaybeIncorrect));
        assert!(help.rendered.is_empty());
        assert_eq!(help.explanation, None);
    }

    #[test]
    fn facts_are_their_query_keys_with_what_they_stand_for() {
        let fact = Fact::Signature(ItemKey(blaze_ids::ContentId([7; 32])));
        let mut diag = Diag::error(Some("E0061"), "this function takes 1 argument", span(29, 32));
        diag.fact(fact, "the signature of `f`");
        let it = diagnostic(&diag, &sources(), Style::default(), None);
        assert_eq!(it.facts, [FactRead { key: fact.key(), what: "the signature of `f`".to_owned() }]);
    }
}
