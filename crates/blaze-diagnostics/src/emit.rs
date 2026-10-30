//! Diagnostics written as rustc writes them: as text (`--error-format=human` and `short`, in
//! colour or not), as `--error-format=json`, and the closing ones after a crate's.
//!
//! Follows rustc's `compiler/rustc_errors/src/annotate_snippet_emitter_writer.rs`, `emitter.rs`
//! (`primary_span_formatted`, `fix_multispans_in_extern_macros_and_render_macro_backtrace`,
//! `is_different`, `detect_confusion_type`, `normalize_whitespace`), `json.rs` and `lib.rs`
//! (`DiagCtxtInner::print_error_count`, `emit_to_destination`), and `compiler/rustc_span/src/
//! lib.rs` (`normalize_src`, `analyze_source_file`, `original_relative_byte_pos`,
//! `macro_backtrace`) at rust-lang/rust ad3d0bc141a02cf446e384136d250a1f6950fed5 (MIT or
//! Apache-2.0), with the version of `annotate-snippets` rustc lays its text out with, and
//! `Renderer::styled()`'s colours as rustc takes them. Generalized from `blaze-lints`' emitter
//! of clippy's lints to every diagnostic rustc gives.

pub mod codes;
mod json;

use std::{collections::BTreeMap, sync::Arc};

use annotate_snippets::{AnnotationKind, Group, Patch, Renderer, Snippet};

use crate::model::{
    Child, Code, Diag, Expansion, Level, MacroKind, MultiSpan, Span, Sublevel, Suggestion,
    SuggestionStyle,
};

/// rustc's `MAX_SUGGESTIONS`: the most ways to substitute that one suggestion shows.
const MAX_SUGGESTIONS: usize = 4;

/// The width rustc lays diagnostics out for when it is not told one, as under cargo.
pub const WIDTH: usize = annotate_snippets::renderer::DEFAULT_TERM_WIDTH;

/// The text of the files diagnostics point into, by the name they give them.
pub struct Sources {
    files: BTreeMap<String, Option<Arc<File>>>,
    read: Box<dyn Fn(&str) -> Option<String> + Send + Sync>,
}

/// A file as rustc's `SourceFile` holds it: its text without a byte order mark and with each
/// `\r\n` as `\n` (`normalize_src`), where its lines start, and where what was taken out was.
/// Spans are in bytes of the file as read; the text is shown and measured as rustc has it.
struct File {
    text: String,
    lines: Vec<usize>,
    /// The length of the file as read.
    len: usize,
    /// The length of the byte order mark taken out, if there was one.
    bom: usize,
    /// Each `\r` taken out: where it was in the file as read, and where its `\n` is in `text`.
    crs: Vec<(usize, usize)>,
}

impl File {
    fn new(read: String) -> File {
        let len = read.len();
        let bom = if read.starts_with('\u{feff}') { '\u{feff}'.len_utf8() } else { 0 };
        let mut text = String::with_capacity(len);
        let mut crs = Vec::new();
        let mut from = bom;
        for (at, _) in read.match_indices("\r\n") {
            text.push_str(&read[from..at]);
            crs.push((at, text.len()));
            from = at + 1;
        }
        text.push_str(&read[from..]);
        let mut lines = vec![0];
        lines.extend(text.match_indices('\n').map(|(at, _)| at + 1).filter(|it| *it < text.len()));
        File { text, lines, len, bom, crs }
    }

    /// Where byte `read` of the file as read is in `text`.
    fn norm(&self, read: u32) -> usize {
        let read = (read as usize).min(self.len);
        let taken = self.crs.partition_point(|(cr, _)| *cr < read);
        floor(&self.text, read.saturating_sub(self.bom) - taken)
    }

    /// rustc's `original_relative_byte_pos`: the byte of the file as read that byte `at` of
    /// `text` is.
    fn original(&self, at: usize) -> u32 {
        let taken = self.crs.partition_point(|(_, lf)| *lf < at);
        (at + self.bom + taken) as u32
    }

    /// Whether `span` is in the file: rustc's `is_valid_span`.
    fn holds(&self, span: &Span) -> bool {
        span.lo <= span.hi && span.hi as usize <= self.len
    }

    /// The 0-based line byte `at` of `text` is on.
    fn line_of(&self, at: usize) -> usize {
        self.lines.partition_point(|it| *it <= at).saturating_sub(1)
    }

    /// The bytes of 0-based line `line`, from its start to the start of the next one.
    fn line_bounds(&self, line: usize) -> std::ops::Range<usize> {
        let start = self.lines.get(line).copied().unwrap_or(self.text.len());
        let end = self.lines.get(line + 1).copied().unwrap_or(self.text.len());
        start..end
    }

    /// The text of 0-based line `line`, without its `\n`: rustc's `get_line`.
    fn line_text(&self, line: usize) -> &str {
        let text = &self.text[self.line_bounds(line)];
        text.strip_suffix('\n').unwrap_or(text)
    }

    /// The 1-based line and the 0-based column in chars of byte `at` of `text`.
    fn position(&self, at: usize) -> (usize, usize) {
        let line = self.line_of(at);
        (line + 1, self.text[self.lines[line]..at].chars().count())
    }

    /// The text of `lo..hi` of `text`.
    fn slice(&self, lo: usize, hi: usize) -> Option<&str> {
        self.text.get(lo..hi)
    }
}

impl Sources {
    /// Files read with `read`, once each.
    pub fn new(read: impl Fn(&str) -> Option<String> + Send + Sync + 'static) -> Sources {
        Sources { files: BTreeMap::new(), read: Box::new(read) }
    }

    /// Files whose text is given.
    pub fn of(files: impl IntoIterator<Item = (String, String)>) -> Sources {
        let mut sources = Sources::new(|_| None);
        for (name, text) in files {
            sources.files.insert(name, Some(Arc::new(File::new(text))));
        }
        sources
    }

    /// The 1-based line and 1-based column (in chars) of byte `at` of `file`.
    pub fn position(&mut self, file: &str, at: u32) -> Option<(u32, u32)> {
        let file = self.file(file)?;
        if at as usize > file.len {
            return None;
        }
        let (line, column) = file.position(file.norm(at));
        Some((line as u32, column as u32 + 1))
    }

    fn file(&mut self, name: &str) -> Option<Arc<File>> {
        if let Some(file) = self.files.get(name) {
            return file.clone();
        }
        let file = (self.read)(name).map(|it| Arc::new(File::new(it)));
        self.files.insert(name.to_owned(), file.clone());
        file
    }

    /// The file `span` is in. One blaze can't read is a bug of whoever made the span: rustc
    /// always has the files its spans point into.
    fn file_of(&mut self, span: &Span) -> Arc<File> {
        self.file(&span.file).unwrap_or_else(|| {
            panic!(
                "blaze-diagnostics: a span at bytes {}..{} of `{}`, a file that can't be read",
                span.lo, span.hi, span.file
            )
        })
    }

    /// The text of `span` as rustc has it, when its file holds it: `span_to_snippet`.
    fn snippet(&mut self, span: &Span) -> Option<String> {
        let file = self.file(&span.file)?;
        if !file.holds(span) {
            return None;
        }
        Some(file.slice(file.norm(span.lo), file.norm(span.hi))?.to_owned())
    }
}

/// How the text of a diagnostic is written.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Style {
    /// `--error-format=short` (`--json=diagnostic-short`): a line of where it is and what it
    /// says.
    pub short: bool,
    /// In colour, as to a terminal (`--color=always`, `--json=diagnostic-rendered-ansi`).
    pub ansi: bool,
    /// The columns it is laid out in (`--diagnostic-width`): rustc takes 140 when not told.
    pub width: usize,
}

impl Default for Style {
    fn default() -> Style {
        Style { short: false, ansi: false, width: WIDTH }
    }
}

/// `diag` as rustc writes it, without the empty line it writes after one.
pub fn rendered(diag: &Diag, sources: &mut Sources, style: Style) -> String {
    render(diag, sources, style, false)
}

/// `rendered` as rustc writes it to stderr: with the empty line after it (none after short, nor
/// after a failure note). Under `short` rustc writes no failure notes (it gives no
/// "For more information ..."), so one is the empty string.
pub fn written(diag: &Diag, sources: &mut Sources, style: Style) -> String {
    write(diag, sources, style, false)
}

/// rustc's `--error-format=json` line of `diag` (keys in rustc's order), without the newline
/// after it, its `rendered` as `style` says (`--json=diagnostic-short`,
/// `--json=diagnostic-rendered-ansi`). Under `short` rustc writes no failure notes, so one is
/// the empty string.
pub fn json(diag: &Diag, sources: &mut Sources, style: Style) -> String {
    json_with(diag, sources, style, false)
}

/// What rustc writes with `-Z ui-testing`, as its UI tests' `.stderr` files have it: line
/// numbers in the margin as `LL`.
pub(crate) fn ui_written(diag: &Diag, sources: &mut Sources, style: Style) -> String {
    write(diag, sources, style, true)
}

/// [`json`] with `-Z ui-testing`.
pub(crate) fn ui_json(diag: &Diag, sources: &mut Sources, style: Style) -> String {
    json_with(diag, sources, style, true)
}

fn write(diag: &Diag, sources: &mut Sources, style: Style, ui_testing: bool) -> String {
    if style.short && diag.level == Level::FailureNote {
        return String::new();
    }
    let mut text = render(diag, sources, style, ui_testing);
    text.push('\n');
    if !style.short && diag.level != Level::FailureNote {
        text.push('\n');
    }
    text
}

fn json_with(diag: &Diag, sources: &mut Sources, style: Style, ui_testing: bool) -> String {
    if style.short && diag.level == Level::FailureNote {
        return String::new();
    }
    let rendered = write(diag, sources, style, ui_testing);
    json::line(diag, sources, rendered)
}

/// rustc's closing diagnostics after a crate's: "aborting due to N previous errors; M warnings
/// emitted", "N warnings emitted", "Some errors have detailed explanations: ...", "For more
/// information about this error, try `rustc --explain E0308`." (failure-note), as rustc's
/// `DiagCtxtInner::print_error_count` gives them.
pub fn summary(diags: &[Diag]) -> Vec<Diag> {
    let errors = diags.iter().filter(|it| it.level == Level::Error).count();
    let warnings = diags.iter().filter(|it| it.level == Level::Warning).count();
    let warned = match warnings {
        0 => String::new(),
        1 => "1 warning emitted".to_owned(),
        count => format!("{count} warnings emitted"),
    };
    let aborted = match errors {
        0 => String::new(),
        1 => "aborting due to 1 previous error".to_owned(),
        count => format!("aborting due to {count} previous errors"),
    };
    let mut closing = Vec::new();
    match (errors, warnings) {
        (0, 0) => return closing,
        (0, _) => closing.push(Diag::new(Level::Warning, None, warned)),
        (_, 0) => closing.push(Diag::new(Level::Error, None, aborted)),
        (_, _) => closing.push(Diag::new(Level::Error, None, format!("{aborted}; {warned}"))),
    }
    let mut explained: Vec<&str> = diags
        .iter()
        .filter_map(|it| match &it.code {
            Some(Code::Error(code)) => Some(code.as_str()),
            _ => None,
        })
        .filter(|code| codes::explanation(code).is_some())
        .collect();
    explained.sort_unstable();
    explained.dedup();
    match &explained[..] {
        [] => {}
        [code] => closing.push(Diag::new(
            Level::FailureNote,
            None,
            format!("For more information about this error, try `rustc --explain {code}`."),
        )),
        [first, ..] => {
            let shown = explained.len().min(9);
            let more = if explained.len() > 9 { "..." } else { "." };
            closing.push(Diag::new(
                Level::FailureNote,
                None,
                format!(
                    "Some errors have detailed explanations: {}{more}",
                    explained[..shown].join(", ")
                ),
            ));
            closing.push(Diag::new(
                Level::FailureNote,
                None,
                format!("For more information about an error, try `rustc --explain {first}`."),
            ));
        }
    }
    closing
}

/// rustc's `AnnotateSnippetEmitter::emit_diagnostic` and `emit_messages_default`.
fn render(diag: &Diag, sources: &mut Sources, style: Style, ui_testing: bool) -> String {
    let mut span = diag.span.clone();
    let mut suggestions: Vec<Suggestion> = match diag.suggestions_disabled {
        true => Vec::new(),
        false => diag.suggestions.clone(),
    };
    primary_span_formatted(&mut span, &mut suggestions, sources);
    let mut children = diag.children.clone();
    let originates = originates_in(diag);
    macro_backtrace(&mut span);
    for child in &mut children {
        macro_backtrace(&mut child.span);
    }
    if let Some(note) = originates
        && !children.iter().any(|it| it.message == note)
    {
        children.push(Child {
            level: Sublevel::Note,
            message: note,
            span: MultiSpan::default(),
            once: false,
        });
    }

    let renderer = match style.ansi {
        true => Renderer::styled(),
        false => Renderer::plain(),
    }
    .term_width(style.width)
    .anonymized_line_numbers(ui_testing)
    .decor_style(annotate_snippets::renderer::DecorStyle::Ascii)
    .short_message(style.short);

    let mut title = level_of(diag.level).primary_title(diag.message.clone());
    if let Some(Code::Error(code)) = &diag.code {
        title = title.id(code.clone());
    }
    let mut report: Vec<Group<'static>> = Vec::new();
    let mut group = Group::with_title(title);

    if span.primary_span().is_some() {
        for (name, file, annotations) in primary_first(collect_annotations(&span, sources), &span) {
            if let Some(snippet) = annotated_snippet(&name, &file, annotations) {
                group = group.element(snippet);
            }
        }
    }

    for child in &children {
        let level = sublevel_of(child.level);
        if child.span.is_empty() {
            group = group.element(level.message(child.message.clone()));
            continue;
        }
        report.push(std::mem::replace(
            &mut group,
            Group::with_title(level.secondary_title(child.message.clone())),
        ));
        if child.span.primary_span().is_some() {
            let files = primary_first(collect_annotations(&child.span, sources), &child.span);
            for (name, file, annotations) in files {
                if let Some(snippet) = annotated_snippet(&name, &file, annotations) {
                    group = group.element(snippet);
                }
            }
        }
    }

    for suggestion in suggestions {
        match suggestion.style {
            SuggestionStyle::CompletelyHidden => {}
            SuggestionStyle::HideCodeAlways => {
                group = group.element(annotate_snippets::Level::HELP.message(suggestion.message));
            }
            SuggestionStyle::HideCodeInline
            | SuggestionStyle::ShowCode
            | SuggestionStyle::ShowAlways => {
                let substitutions = substitutions(suggestion.substitutions, sources);
                let Some((name, _, _)) = substitutions.first() else { continue };
                let name = name.clone();
                let mut message = suggestion.message;
                let others = substitutions.len().saturating_sub(MAX_SUGGESTIONS);
                let mut subs = Vec::new();
                for (_, file, parts) in substitutions.into_iter().take(MAX_SUGGESTIONS) {
                    let confusion = parts.iter().fold(Confusion::None, |it, (lo, hi, snippet)| {
                        it.combine(detect_confusion_type(&file, snippet, *lo, *hi))
                    });
                    message.push_str(confusion.label_text());
                    let mut parts: Vec<(usize, usize, String)> = parts
                        .into_iter()
                        .filter(|(lo, hi, snippet)| is_different(&file, snippet, *lo, *hi))
                        .collect();
                    if parts.is_empty() {
                        continue;
                    }
                    let fold = !attribute_suggestion(&file, &mut parts);
                    let ranges: Vec<(usize, usize)> = parts.iter().map(|it| (it.0, it.1)).collect();
                    let Some((start, source, line)) = shrink_file(&file, &ranges) else { continue };
                    subs.push(
                        Snippet::source(source)
                            .line_start(line)
                            .path(name.clone())
                            .fold(fold)
                            .patches(parts.into_iter().map(move |(lo, hi, replacement)| {
                                Patch::new(
                                    lo.saturating_sub(start)..hi.saturating_sub(start),
                                    replacement,
                                )
                            })),
                    );
                }
                if !subs.is_empty() {
                    report.push(std::mem::replace(
                        &mut group,
                        Group::with_title(annotate_snippets::Level::HELP.secondary_title(message)),
                    ));
                    group = group.elements(subs);
                    if others > 0 {
                        let plural = if others == 1 { "" } else { "s" };
                        group = group.element(
                            annotate_snippets::Level::NOTE
                                .no_name()
                                .message(format!("and {others} other candidate{plural}")),
                        );
                    }
                }
            }
        }
    }
    if !group.is_empty() {
        report.push(group);
    }
    renderer.render(&report)
}

/// rustc's `annotation_level_for_level`.
fn level_of(level: Level) -> annotate_snippets::Level<'static> {
    match level {
        Level::Error => annotate_snippets::Level::ERROR,
        Level::Warning => annotate_snippets::Level::WARNING,
        Level::Note => annotate_snippets::Level::NOTE,
        Level::Help => annotate_snippets::Level::HELP,
        Level::FailureNote => annotate_snippets::Level::NOTE.no_name(),
    }
}

fn sublevel_of(level: Sublevel) -> annotate_snippets::Level<'static> {
    match level {
        Sublevel::Note => annotate_snippets::Level::NOTE,
        Sublevel::Help => annotate_snippets::Level::HELP,
        Sublevel::Warning => annotate_snippets::Level::WARNING,
    }
}

/// A label placed in its file: bytes of the file's text as rustc has it.
struct Annotation {
    kind: AnnotationKind,
    lo: usize,
    hi: usize,
    label: Option<String>,
}

/// rustc's `collect_annotations`: the labels of `span` by file in the order they are met, each
/// file's by line, leaving out those not in their file.
fn collect_annotations(
    span: &MultiSpan,
    sources: &mut Sources,
) -> Vec<(String, Arc<File>, Vec<Annotation>)> {
    let mut output: Vec<(String, Arc<File>, Vec<Annotation>)> = Vec::new();
    for label in span.span_labels() {
        let file = sources.file_of(label.span);
        if !file.holds(label.span) {
            continue;
        }
        let annotation = Annotation {
            kind: if label.is_primary { AnnotationKind::Primary } else { AnnotationKind::Context },
            lo: file.norm(label.span.lo),
            hi: file.norm(label.span.hi),
            label: label.label.map(normalize_whitespace),
        };
        match output.iter_mut().find(|(name, ..)| *name == label.span.file) {
            Some((.., annotations)) => annotations.push(annotation),
            None => output.push((label.span.file.clone(), file, vec![annotation])),
        }
    }
    for (_, file, annotations) in &mut output {
        annotations.sort_by_key(|it| file.line_of(it.lo));
    }
    output
}

/// The file of the primary span first, found as rustc finds it: by a binary search of the
/// files in the order they were met.
fn primary_first(
    mut files: Vec<(String, Arc<File>, Vec<Annotation>)>,
    span: &MultiSpan,
) -> Vec<(String, Arc<File>, Vec<Annotation>)> {
    if let Some(primary) = span.primary_span()
        && let Ok(at) = files.binary_search_by(|(name, ..)| name.as_str().cmp(&primary.file))
    {
        files.swap(0, at);
    }
    files
}

fn annotated_snippet(
    name: &str,
    file: &File,
    annotations: Vec<Annotation>,
) -> Option<Snippet<'static, annotate_snippets::Annotation<'static>>> {
    let ranges: Vec<(usize, usize)> = annotations.iter().map(|it| (it.lo, it.hi)).collect();
    let (start, source, line) = shrink_file(file, &ranges)?;
    Some(Snippet::source(source).line_start(line).path(name.to_owned()).annotations(
        annotations.into_iter().map(move |it| {
            let annotation = it.kind.span(it.lo.saturating_sub(start)..it.hi.saturating_sub(start));
            match it.label {
                Some(label) => annotation.label(label),
                None => annotation,
            }
        }),
    ))
}

/// rustc's `shrink_file`: the whole lines `ranges` are on, where they start, and the line
/// number of the first.
fn shrink_file(file: &File, ranges: &[(usize, usize)]) -> Option<(usize, String, usize)> {
    let lo = ranges.iter().map(|it| it.0).min()?;
    let hi = ranges.iter().map(|it| it.1).max()?;
    let (lo_line, hi_line) = (file.line_of(lo), file.line_of(hi));
    let start = file.line_bounds(lo_line).start;
    let end = file.line_bounds(hi_line).end;
    Some((start, file.text.get(start..end)?.to_owned(), lo_line + 1))
}

/// The ways to substitute of a suggestion that rustc shows: those whose parts are all in one
/// file it holds them in, each with its parts in order, leaving out the parts that change
/// nothing, and the ways left with no part.
fn substitutions(
    substitutions: Vec<Vec<(Span, String)>>,
    sources: &mut Sources,
) -> Vec<(String, Arc<File>, Vec<(usize, usize, String)>)> {
    substitutions
        .into_iter()
        .filter_map(|parts| {
            let name = parts.first()?.0.file.clone();
            if parts.iter().any(|(span, _)| span.file != name) {
                return None;
            }
            let file = sources.file(&name)?;
            if parts.iter().any(|(span, _)| !file.holds(span)) {
                return None;
            }
            let mut parts: Vec<(usize, usize, String)> = parts
                .into_iter()
                .map(|(span, snippet)| (file.norm(span.lo), file.norm(span.hi), snippet))
                .collect();
            parts.sort_by_key(|it| it.0);
            parts.retain(|(lo, hi, snippet)| is_different(&file, snippet, *lo, *hi));
            (!parts.is_empty()).then_some((name, file, parts))
        })
        .collect()
}

/// rustc's attribute special case: a suggestion that adds `#[...]` on a line of its own is
/// moved to the start of the line, with its indentation, and shown with the line after.
fn attribute_suggestion(file: &File, parts: &mut [(usize, usize, String)]) -> bool {
    let [(lo, hi, snippet)] = parts else { return false };
    let trimmed = snippet.trim();
    if !(trimmed.starts_with("#[") && trimmed.ends_with(']') && snippet.ends_with('\n')) {
        return false;
    }
    if lo != hi {
        return false;
    }
    let before = &file.text[..*lo];
    let before = before.rsplit_once('\n').map_or(before, |(_, it)| it);
    if !before.trim().is_empty() {
        return false;
    }
    if !before.is_empty() && !snippet.ends_with(before) {
        snippet.insert_str(0, before);
        *lo -= before.len();
        *hi = *lo;
    }
    true
}

/// rustc's `Span::macro_backtrace`: the expansions `span` is in, innermost first, leaving out
/// a macro's recursive calls of itself.
fn backtrace(span: &Span) -> Vec<&Expansion> {
    let mut expansions = Vec::new();
    let mut prev: Option<&Span> = None;
    let mut at = span;
    while let Some(expansion) = at.expansion.as_deref() {
        let recursive = prev.is_some_and(|it| same_place(&expansion.call_site, it));
        prev = Some(at);
        at = &expansion.call_site;
        if !recursive {
            expansions.push(expansion);
        }
    }
    expansions
}

/// rustc's `Span::source_equal`.
fn same_place(a: &Span, b: &Span) -> bool {
    a.file == b.file && a.lo == b.lo && a.hi == b.hi
}

/// rustc's `render_multispan_macro_backtrace` without `-Z macro-backtrace`: a primary span in a
/// macro's expansion that is not in its call gets a label on the outermost call of a macro
/// whose definition rustc knows.
fn macro_backtrace(span: &mut MultiSpan) {
    let mut labels: Vec<(Span, String)> = Vec::new();
    for primary in &span.primary {
        for expansion in backtrace(primary).into_iter().rev() {
            if expansion.def_site.is_none() {
                continue;
            }
            let call = &expansion.call_site;
            let contains = call.file == primary.file && call.lo <= primary.lo && primary.hi <= call.hi;
            if !contains {
                let what = match expansion.kind {
                    MacroKind::Attr => "this attribute macro expansion",
                    MacroKind::Derive => "this derive macro expansion",
                    MacroKind::Bang => "this macro invocation",
                };
                let label = (call.clone(), format!("in {what}"));
                if !labels.contains(&label) {
                    labels.push(label);
                }
            }
            break;
        }
    }
    span.labels.extend(labels);
}

/// rustc's note on a diagnostic in a macro's expansion, which its emitter adds: the macro it
/// originates in, from the spans of it and of its children
/// (`fix_multispans_in_extern_macros_and_render_macro_backtrace`).
fn originates_in(diag: &Diag) -> Option<String> {
    let spans = diag.span.primary.iter().chain(diag.children.iter().flat_map(|it| &it.span.primary));
    let macros: Vec<&Expansion> = spans.flat_map(backtrace).collect();
    let (first, last) = (macros.first()?, macros.last()?);
    let and_then = match last.name != first.name {
        true => {
            format!(" which comes from the expansion of the {} `{}`", last.kind.descr(), last.name)
        }
        false => String::new(),
    };
    Some(format!(
        "this {} originates in the {} `{}`{and_then} (in Nightly builds, run with -Z macro-backtrace for more info)",
        diag.level.name(),
        first.kind.descr(),
        first.name
    ))
}

/// rustc's `primary_span_formatted`: a lone suggestion that is short is shown as a label on
/// what it replaces, and not as a patch.
fn primary_span_formatted(
    span: &mut MultiSpan,
    suggestions: &mut Vec<Suggestion>,
    sources: &mut Sources,
) {
    let [suggestion] = &suggestions[..] else { return };
    let [substitution] = &suggestion.substitutions[..] else { return };
    let [(part_span, snippet)] = &substitution[..] else { return };
    let inline = suggestion.message.split_whitespace().count() < 10
        && !snippet.contains('\n')
        && !matches!(
            suggestion.style,
            SuggestionStyle::HideCodeAlways
                | SuggestionStyle::CompletelyHidden
                | SuggestionStyle::ShowAlways
        );
    if !inline {
        return;
    }
    let snippet = snippet.trim();
    let label = if snippet.is_empty() || suggestion.style.hide_inline() {
        format!("help: {}", suggestion.message)
    } else {
        let confusion = match sources.snippet(part_span) {
            Some(found) => confusion_of(&found, snippet),
            None => Confusion::None,
        };
        format!("help: {}{}: `{snippet}`", suggestion.message, confusion.label_text())
    };
    span.labels.push((part_span.clone(), label));
    suggestions.clear();
}

/// rustc's `is_different`: whether `suggested` is not what is at `lo..hi` already.
fn is_different(file: &File, suggested: &str, lo: usize, hi: usize) -> bool {
    file.slice(lo, hi).is_none_or(|found| found != suggested)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Confusion {
    None,
    Case,
    DigitLetter,
    Both,
}

impl Confusion {
    fn label_text(self) -> &'static str {
        match self {
            Confusion::None => "",
            Confusion::Case => " (notice the capitalization)",
            Confusion::DigitLetter => " (notice the digit/letter confusion)",
            Confusion::Both => " (notice the capitalization and digit/letter confusion)",
        }
    }

    fn combine(self, other: Confusion) -> Confusion {
        match (self, other) {
            (Confusion::None, other) => other,
            (this, Confusion::None) => this,
            (Confusion::Both, _) | (_, Confusion::Both) => Confusion::Both,
            (Confusion::Case, Confusion::DigitLetter)
            | (Confusion::DigitLetter, Confusion::Case) => Confusion::Both,
            (this, _) => this,
        }
    }
}

/// rustc's `detect_confusion_type` of the text at `lo..hi`.
fn detect_confusion_type(file: &File, suggested: &str, lo: usize, hi: usize) -> Confusion {
    match file.slice(lo, hi) {
        Some(found) => confusion_of(found, suggested),
        None => Confusion::None,
    }
}

/// Whether `suggested` differs from `found` only in letters that look alike.
fn confusion_of(found: &str, suggested: &str) -> Confusion {
    if found.len() != suggested.len() {
        return Confusion::None;
    }
    const ASCII_CONFUSABLES: &[char] =
        &['c', 'f', 'i', 'k', 'o', 's', 'u', 'v', 'w', 'x', 'y', 'z'];
    const DIGIT_LETTER: [(char, char); 5] =
        [('0', 'O'), ('1', 'l'), ('5', 'S'), ('8', 'B'), ('9', 'g')];
    let (mut case, mut digit_letter, mut other) = (false, false, false);
    for (f, s) in std::iter::zip(found.chars(), suggested.chars()) {
        if f == s {
            continue;
        }
        if f.eq_ignore_ascii_case(&s) {
            if ASCII_CONFUSABLES.contains(&f) || ASCII_CONFUSABLES.contains(&s) {
                case = true;
            } else {
                other = true;
            }
        } else if DIGIT_LETTER.contains(&(f, s)) || DIGIT_LETTER.contains(&(s, f)) {
            digit_letter = true;
        } else {
            other = true;
        }
    }
    let differs = found != suggested;
    match (case && !other && differs, digit_letter && !other && differs) {
        (true, true) => Confusion::Both,
        (true, false) => Confusion::Case,
        (false, true) => Confusion::DigitLetter,
        (false, false) => Confusion::None,
    }
}

/// rustc's `OUTPUT_REPLACEMENTS`, in its order.
const OUTPUT_REPLACEMENTS: &[(char, &str)] = &[
    ('\0', "␀"),
    ('\u{0001}', "␁"),
    ('\u{0002}', "␂"),
    ('\u{0003}', "␃"),
    ('\u{0004}', "␄"),
    ('\u{0005}', "␅"),
    ('\u{0006}', "␆"),
    ('\u{0007}', "␇"),
    ('\u{0008}', "␈"),
    ('\t', "    "),
    ('\u{000b}', "␋"),
    ('\u{000c}', "␌"),
    ('\u{000d}', "␍"),
    ('\u{000e}', "␎"),
    ('\u{000f}', "␏"),
    ('\u{0010}', "␐"),
    ('\u{0011}', "␑"),
    ('\u{0012}', "␒"),
    ('\u{0013}', "␓"),
    ('\u{0014}', "␔"),
    ('\u{0015}', "␕"),
    ('\u{0016}', "␖"),
    ('\u{0017}', "␗"),
    ('\u{0018}', "␘"),
    ('\u{0019}', "␙"),
    ('\u{001a}', "␚"),
    ('\u{001b}', "␛"),
    ('\u{001c}', "␜"),
    ('\u{001d}', "␝"),
    ('\u{001e}', "␞"),
    ('\u{001f}', "␟"),
    ('\u{007f}', "␡"),
    ('\u{200d}', ""),
    ('\u{202a}', "�"),
    ('\u{202b}', "�"),
    ('\u{202c}', "�"),
    ('\u{202d}', "�"),
    ('\u{202e}', "�"),
    ('\u{2066}', "�"),
    ('\u{2067}', "�"),
    ('\u{2068}', "�"),
    ('\u{2069}', "�"),
];

fn normalize_whitespace(text: &str) -> String {
    text.chars().fold(String::with_capacity(text.len()), |mut out, c| {
        match OUTPUT_REPLACEMENTS.binary_search_by_key(&c, |(it, _)| *it) {
            Ok(at) => out.push_str(OUTPUT_REPLACEMENTS[at].1),
            Err(_) => out.push(c),
        }
        out
    })
}

/// The last char boundary of `text` at or before `at`.
fn floor(text: &str, at: usize) -> usize {
    let mut at = at.min(text.len());
    while !text.is_char_boundary(at) {
        at -= 1;
    }
    at
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_file_is_measured_as_rustc_has_it_without_its_bom_and_crs() {
        let file = File::new("\u{feff}a\r\nbc\r\n\r\nd".to_owned());
        assert_eq!(file.text, "a\nbc\n\nd");
        assert_eq!(file.lines, [0, 2, 5, 6]);
        // `b`, after the BOM (3), `a` and `\r\n`.
        assert_eq!(file.norm(6), 2);
        assert_eq!(file.original(2), 6);
        // The end of `a`, where `\r` is, is the `\n` of the text, and back.
        assert_eq!(file.norm(4), 1);
        assert_eq!(file.original(1), 4);
        assert_eq!(file.norm(0), 0);
        assert_eq!(file.original(0), 3);
        assert_eq!(file.position(file.norm(6)), (2, 0));
    }

    #[test]
    fn a_trailing_newline_starts_no_line() {
        let file = File::new("ab\ncd\n".to_owned());
        assert_eq!(file.lines, [0, 3]);
        assert_eq!(file.position(6), (2, 3));
    }

    #[test]
    fn positions_are_one_based_in_chars() {
        let mut sources = Sources::of([("a.rs".to_owned(), "é = 1;\n  x".to_owned())]);
        assert_eq!(sources.position("a.rs", 2), Some((1, 2)));
        assert_eq!(sources.position("a.rs", 10), Some((2, 3)));
        assert_eq!(sources.position("a.rs", 11), Some((2, 4)));
        assert_eq!(sources.position("a.rs", 12), None);
        assert_eq!(sources.position("b.rs", 0), None);
    }

    #[test]
    fn files_are_read_once_each() {
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counted = reads.clone();
        let mut sources = Sources::new(move |name| {
            counted.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            (name == "a.rs").then(|| "fn f() {}\n".to_owned())
        });
        assert_eq!(sources.position("a.rs", 3), Some((1, 4)));
        assert_eq!(sources.position("a.rs", 4), Some((1, 5)));
        assert_eq!(sources.position("b.rs", 0), None);
        assert_eq!(sources.position("b.rs", 0), None);
        assert_eq!(reads.load(std::sync::atomic::Ordering::Relaxed), 2);
    }

    #[test]
    fn confusables_are_noticed() {
        assert_eq!(confusion_of("Vec", "vec"), Confusion::Case);
        assert_eq!(confusion_of("l0", "1O"), Confusion::DigitLetter);
        assert_eq!(confusion_of("so", "S0"), Confusion::Both);
        assert_eq!(confusion_of("abc", "abd"), Confusion::None);
    }

    fn expanded(lo: u32, hi: u32, name: &str, call_site: Span) -> Span {
        Span {
            file: "a.rs".to_owned(),
            lo,
            hi,
            expansion: Some(Box::new(Expansion {
                call_site,
                kind: MacroKind::Bang,
                name: name.to_owned(),
                def_site: None,
            })),
        }
    }

    #[test]
    fn the_backtrace_is_innermost_first() {
        let root = Span { file: "a.rs".to_owned(), lo: 0, hi: 10, expansion: None };
        let at = expanded(3, 4, "inner", expanded(20, 30, "outer", root));
        let names: Vec<&str> = backtrace(&at).iter().map(|it| it.name.as_str()).collect();
        assert_eq!(names, ["inner", "outer"]);
    }

    #[test]
    fn a_call_where_the_span_two_steps_back_is_is_left_out_of_the_backtrace() {
        let root = Span { file: "a.rs".to_owned(), lo: 3, hi: 4, expansion: None };
        let at = expanded(3, 4, "m", expanded(20, 30, "m", root));
        let names: Vec<&str> = backtrace(&at).iter().map(|it| it.name.as_str()).collect();
        assert_eq!(names, ["m"]);
    }
}
