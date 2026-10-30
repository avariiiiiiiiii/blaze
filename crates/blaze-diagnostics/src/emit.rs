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
//!
//! Each file is read and indexed once, into an arena of [`Sources`] by `u32`; each diagnostic
//! looks its files up once; what is rendered borrows the text of the files and of the
//! diagnostic, and copies none of it. The bar is rustc's own: its `analyze_source_file` finds
//! lines 16 bytes at a time with SSE2, where `memchr` picks AVX2, SSE2 or NEON at run time, and
//! the layout is the same `annotate-snippets` rustc renders with, given borrowed text where
//! rustc formats each line into a `String` first.

pub mod codes;
mod json;

use std::borrow::Cow;

use annotate_snippets::{AnnotationKind, Group, Patch, Renderer, Snippet};
use blaze_ids::radix;
use rustc_hash::FxBuildHasher;
use smallvec::{SmallVec, smallvec};

use crate::model::{Code, Diag, Expansion, Level, MacroKind, MultiSpan, Span, Sublevel, Suggestion, SuggestionStyle};

/// rustc's `MAX_SUGGESTIONS`: the most ways to substitute that one suggestion shows.
const MAX_SUGGESTIONS: usize = 4;

/// The width rustc lays diagnostics out for when it is not told one, as under cargo.
pub const WIDTH: usize = annotate_snippets::renderer::DEFAULT_TERM_WIDTH;

/// The index of a file that can't be read: past the end of every arena of files.
const UNREAD: u32 = u32::MAX;

/// The text of the files diagnostics point into, by the name they give them.
pub struct Sources {
    /// Each file's index in `files` by its name, or [`UNREAD`]: one probe of an Fx table per
    /// lookup, allocating only the first time a name is met.
    names: hashbrown::HashMap<String, u32, FxBuildHasher>,
    files: Vec<File>,
    read: Box<dyn Fn(&str) -> Option<String> + Send + Sync>,
}

/// A file as rustc's `SourceFile` holds it: its text without a byte order mark and with each
/// `\r\n` as `\n` (`normalize_src`), where its lines start, and where what was taken out was.
/// Spans are in bytes of the file as read; the text is shown and measured as rustc has it.
///
/// The text is the `String` read, not a copy, unless a BOM or a `\r\n` is taken out. 4 bytes a
/// line and 8 a `\r\n` besides; placing a byte is a binary search of the lines, then a count of
/// the line's chars only in a file that is not ASCII (rustc keeps its multibyte chars for it).
struct File {
    text: String,
    lines: Vec<u32>,
    /// Each `\r` taken out: where it was in the file as read, and where its `\n` is in `text`.
    crs: Vec<(u32, u32)>,
    /// The length of the file as read.
    len: u32,
    /// The length of the byte order mark taken out, if there was one.
    bom: u32,
    ascii: bool,
}

impl File {
    fn new(read: String) -> File {
        let len = read.len() as u32;
        let bom = if read.starts_with('\u{feff}') { '\u{feff}'.len_utf8() } else { 0 };
        let mut crs = Vec::new();
        let text = match bom == 0 && memchr::memmem::find(read.as_bytes(), b"\r\n").is_none() {
            true => read,
            false => {
                let mut text = String::with_capacity(read.len() - bom);
                let mut from = bom;
                for at in memchr::memmem::find_iter(read.as_bytes(), b"\r\n") {
                    text.push_str(&read[from..at]);
                    crs.push((at as u32, text.len() as u32));
                    from = at + 1;
                }
                text.push_str(&read[from..]);
                text
            }
        };
        let bytes = text.as_bytes();
        let mut lines = Vec::with_capacity(memchr::memchr_iter(b'\n', bytes).count() + 1);
        lines.push(0);
        lines.extend(
            memchr::memchr_iter(b'\n', bytes)
                .map(|at| at as u32 + 1)
                .filter(|it| (*it as usize) < bytes.len()),
        );
        let ascii = text.is_ascii();
        File { text, lines, crs, len, bom: bom as u32, ascii }
    }

    /// Where byte `read` of the file as read is in `text`.
    fn norm(&self, read: u32) -> usize {
        let read = read.min(self.len);
        let taken = self.crs.partition_point(|(cr, _)| *cr < read) as u32;
        floor(&self.text, (read.saturating_sub(self.bom) - taken) as usize)
    }

    /// rustc's `original_relative_byte_pos`: the byte of the file as read that byte `at` of
    /// `text` is.
    fn original(&self, at: usize) -> u32 {
        let taken = self.crs.partition_point(|(_, lf)| (*lf as usize) < at) as u32;
        at as u32 + self.bom + taken
    }

    /// Whether `span` is in the file: rustc's `is_valid_span`.
    fn holds(&self, span: &Span) -> bool {
        span.lo <= span.hi && span.hi <= self.len
    }

    /// The 0-based line byte `at` of `text` is on.
    fn line_of(&self, at: usize) -> usize {
        self.lines.partition_point(|it| (*it as usize) <= at).saturating_sub(1)
    }

    /// The bytes of 0-based line `line`, from its start to the start of the next one.
    fn line_bounds(&self, line: usize) -> std::ops::Range<usize> {
        let start = self.lines.get(line).map_or(self.text.len(), |it| *it as usize);
        let end = self.lines.get(line + 1).map_or(self.text.len(), |it| *it as usize);
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
        let start = self.lines[line] as usize;
        let column = match self.ascii {
            true => at - start,
            false => self.text[start..at].chars().count(),
        };
        (line + 1, column)
    }

    /// The text of `lo..hi` of `text`.
    fn slice(&self, lo: usize, hi: usize) -> Option<&str> {
        self.text.get(lo..hi)
    }

    /// The text of `span` as rustc has it, when the file holds it: `span_to_snippet`.
    fn snippet(&self, span: &Span) -> Option<&str> {
        match self.holds(span) {
            true => self.slice(self.norm(span.lo), self.norm(span.hi)),
            false => None,
        }
    }
}

impl Sources {
    /// Files read with `read`, once each.
    pub fn new(read: impl Fn(&str) -> Option<String> + Send + Sync + 'static) -> Sources {
        Sources { names: hashbrown::HashMap::default(), files: Vec::new(), read: Box::new(read) }
    }

    /// Files whose text is given.
    pub fn of(files: impl IntoIterator<Item = (String, String)>) -> Sources {
        let mut sources = Sources::new(|_| None);
        for (name, text) in files {
            sources.names.insert(name, sources.files.len() as u32);
            sources.files.push(File::new(text));
        }
        sources
    }

    /// The 1-based line and 1-based column (in chars) of byte `at` of `file`.
    pub fn position(&mut self, file: &str, at: u32) -> Option<(u32, u32)> {
        let index = self.load(file);
        let file = self.files.get(index as usize)?;
        if at > file.len {
            return None;
        }
        let (line, column) = file.position(file.norm(at));
        Some((line as u32, column as u32 + 1))
    }

    /// The index of the file `name`, read the first time it is asked for.
    fn load(&mut self, name: &str) -> u32 {
        let Sources { names, files, read } = self;
        *names.entry_ref(name).or_insert_with(|| match read(name) {
            Some(text) => {
                files.push(File::new(text));
                (files.len() - 1) as u32
            }
            None => UNREAD,
        })
    }
}

/// The files one diagnostic points into, each looked up in [`Sources`] once. A diagnostic has
/// one or two, so a scan comparing names costs less than hashing them again.
struct Placed<'a> {
    names: SmallVec<[(&'a str, u32); 4]>,
    files: &'a [File],
}

impl<'a> Placed<'a> {
    fn new(diag: &'a Diag, sources: &'a mut Sources) -> Placed<'a> {
        let mut names = SmallVec::new();
        let spans = spans_of(&diag.span)
            .chain(diag.children.iter().flat_map(|it| spans_of(&it.span)))
            .chain(
                diag.suggestions
                    .iter()
                    .flat_map(|it| it.substitutions.iter().flatten().map(|(span, _)| span)),
            );
        for span in spans {
            meet(&mut names, sources, span);
        }
        let sources: &'a Sources = sources;
        Placed { names, files: &sources.files }
    }

    fn get(&self, name: &str) -> Option<&'a File> {
        let &(_, index) = self.names.iter().find(|(it, _)| *it == name)?;
        self.files.get(index as usize)
    }

    /// The file `span` is in. One blaze can't read is a bug of whoever made the span: rustc
    /// always has the files its spans point into.
    fn file_of(&self, span: &Span) -> &'a File {
        self.file_named(&span.file, span.lo, span.hi)
    }

    /// [`Placed::file_of`] a span at `lo..hi` of `name`.
    fn file_named(&self, name: &str, lo: u32, hi: u32) -> &'a File {
        self.get(name).unwrap_or_else(|| {
            panic!("blaze-diagnostics: a span at bytes {lo}..{hi} of `{name}`, a file that can't be read")
        })
    }
}

fn spans_of(span: &MultiSpan) -> impl Iterator<Item = &Span> {
    span.primary.iter().chain(span.labels.iter().map(|(it, _)| it))
}

/// Looks up the file of `span` and of each macro call and definition it is in, once each.
fn meet<'a>(names: &mut SmallVec<[(&'a str, u32); 4]>, sources: &mut Sources, mut span: &'a Span) {
    loop {
        if !names.iter().any(|(it, _)| *it == span.file) {
            names.push((span.file.as_str(), sources.load(&span.file)));
        }
        let Some(expansion) = span.expansion.as_deref() else { return };
        if let Some(def) = &expansion.def_site {
            meet(names, sources, def);
        }
        span = &expansion.call_site;
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
    /// `-Z ui-testing`, as rustc's UI tests' `.stderr` files have it: line numbers in the
    /// margin as `LL`.
    pub ui_testing: bool,
}

impl Default for Style {
    fn default() -> Style {
        Style { short: false, ansi: false, width: WIDTH, ui_testing: false }
    }
}

/// `diag` as rustc writes it, without the empty line it writes after one.
pub fn rendered(diag: &Diag, sources: &mut Sources, style: Style) -> String {
    render(diag, &Placed::new(diag, sources), style)
}

/// `rendered` as rustc writes it to stderr: with the empty line after it (none after short, nor
/// after a failure note). Under `short` rustc names no error codes to explain (its emitter
/// can't show them), so one of [`summary`]'s notes naming them is the empty string.
pub fn written(diag: &Diag, sources: &mut Sources, style: Style) -> String {
    if style.short && explains(diag) {
        return String::new();
    }
    write(diag, &Placed::new(diag, sources), style)
}

/// rustc's `--error-format=json` line of `diag` (keys in rustc's order), without the newline
/// after it, its `rendered` as `style` says (`--json=diagnostic-short`,
/// `--json=diagnostic-rendered-ansi`). Under `short`, one of [`summary`]'s notes naming error
/// codes to explain is the empty string, as in [`written`].
pub fn json(diag: &Diag, sources: &mut Sources, style: Style) -> String {
    if style.short && explains(diag) {
        return String::new();
    }
    let placed = Placed::new(diag, sources);
    let rendered = write(diag, &placed, style);
    json::line(diag, &placed, rendered)
}

fn write(diag: &Diag, placed: &Placed<'_>, style: Style) -> String {
    let mut text = render(diag, placed, style);
    text.push('\n');
    if !style.short && diag.level != Level::FailureNote {
        text.push('\n');
    }
    text
}

/// Whether `diag` is one of the closing notes naming error codes to explain, which rustc's
/// `print_error_count` leaves out when its emitter can't show them (`should_show_explain`).
fn explains(diag: &Diag) -> bool {
    diag.level == Level::FailureNote
        && diag.code.is_none()
        && (diag.message.starts_with("Some errors have detailed explanations: ")
            || diag.message.starts_with("For more information about this error, try `rustc --explain ")
            || diag.message.starts_with("For more information about an error, try `rustc --explain "))
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
    let mut closing = Vec::with_capacity(3);
    match (errors, warnings) {
        (0, 0) => return closing,
        (0, _) => closing.push(Diag::new(Level::Warning, None, warned)),
        (_, 0) => closing.push(Diag::new(Level::Error, None, aborted)),
        (_, _) => closing.push(Diag::new(Level::Error, None, format!("{aborted}; {warned}"))),
    }
    let mut explained: Vec<&str> = diags
        .iter()
        .filter_map(|it| it.code.as_ref().and_then(Code::error))
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

/// Labels the emitter adds to a span: the short suggestion shown inline and the macro calls.
type Labels<'a> = SmallVec<[(&'a Span, Cow<'a, str>); 2]>;

/// rustc's `AnnotateSnippetEmitter::emit_diagnostic` and `emit_messages_default`.
fn render<'a>(diag: &'a Diag, placed: &Placed<'a>, style: Style) -> String {
    let mut suggestions: &'a [Suggestion] = match diag.suggestions_disabled {
        true => &[],
        false => &diag.suggestions,
    };
    let mut extra: Labels<'a> = SmallVec::new();
    if let Some(label) = primary_span_formatted(suggestions, placed) {
        extra.push(label);
        suggestions = &[];
    }
    macro_labels(&diag.span, &mut extra);

    let renderer = match style.ansi {
        true => Renderer::styled(),
        false => Renderer::plain(),
    }
    .term_width(style.width)
    .anonymized_line_numbers(style.ui_testing)
    .decor_style(annotate_snippets::renderer::DecorStyle::Ascii)
    .short_message(style.short);

    let mut title = level_of(diag.level).primary_title(diag.message.as_str());
    if let Some(code) = diag.code.as_ref().and_then(Code::error) {
        title = title.id(code);
    }
    let mut report: Vec<Group<'a>> = Vec::with_capacity(2 + diag.children.len() + suggestions.len());
    let mut group = Group::with_title(title);

    if diag.span.primary_span().is_some() {
        group = snippets(group, &diag.span, extra, placed);
    }

    for child in &diag.children {
        let level = sublevel_of(child.level);
        if child.span.is_empty() {
            group = group.element(level.message(child.message.as_str()));
            continue;
        }
        report.push(std::mem::replace(
            &mut group,
            Group::with_title(level.secondary_title(child.message.as_str())),
        ));
        if child.span.primary_span().is_some() {
            let mut extra: Labels<'a> = SmallVec::new();
            macro_labels(&child.span, &mut extra);
            group = snippets(group, &child.span, extra, placed);
        }
    }
    if let Some(note) = originates_in(diag)
        && !diag.children.iter().any(|it| it.message == note)
    {
        group = group.element(annotate_snippets::Level::NOTE.message(note));
    }

    for suggestion in suggestions {
        match suggestion.style {
            SuggestionStyle::CompletelyHidden => {}
            SuggestionStyle::HideCodeAlways => {
                group = group
                    .element(annotate_snippets::Level::HELP.message(suggestion.message.as_str()));
            }
            SuggestionStyle::HideCodeInline
            | SuggestionStyle::ShowCode
            | SuggestionStyle::ShowAlways => {
                let substitutions = substitutions(&suggestion.substitutions, placed);
                let Some(&(name, ..)) = substitutions.first() else { continue };
                let mut message = Cow::Borrowed(suggestion.message.as_str());
                let others = substitutions.len().saturating_sub(MAX_SUGGESTIONS);
                let mut subs = Vec::with_capacity(substitutions.len().min(MAX_SUGGESTIONS));
                for (_, file, mut parts) in substitutions.into_iter().take(MAX_SUGGESTIONS) {
                    let confusion = parts.iter().fold(Confusion::None, |it, part| {
                        it.combine(detect_confusion_type(file, &part.snippet, part.lo, part.hi))
                    });
                    if confusion != Confusion::None {
                        message.to_mut().push_str(confusion.label_text());
                    }
                    let fold = !attribute_suggestion(file, &mut parts);
                    let Some((start, end, line)) = shrink_file(file, parts.iter().map(|it| (it.lo, it.hi)))
                    else {
                        continue;
                    };
                    subs.push(
                        Snippet::source(&file.text[start..end])
                            .line_start(line)
                            .path(name)
                            .fold(fold)
                            .patches(parts.into_iter().map(move |it| {
                                Patch::new(
                                    it.lo.saturating_sub(start)..it.hi.saturating_sub(start),
                                    it.snippet,
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

/// A label placed in its file: bytes of the file's text as rustc has it, and the line they
/// start on.
struct Annotation<'a> {
    kind: AnnotationKind,
    lo: usize,
    hi: usize,
    line: usize,
    label: Option<Cow<'a, str>>,
}

/// The labels of a span by file, each file's in the order rustc shows them.
type Annotated<'a> = SmallVec<[(&'a str, &'a File, SmallVec<[Annotation<'a>; 4]>); 2]>;

/// The snippets of `span` with `extra` labels on it, the file of its primary span first.
fn snippets<'a>(
    mut group: Group<'a>,
    span: &'a MultiSpan,
    extra: Labels<'a>,
    placed: &Placed<'a>,
) -> Group<'a> {
    let mut files = collect_annotations(span, extra, placed);
    // As rustc finds it: by a binary search of the files in the order they were met.
    if let Some(primary) = span.primary_span()
        && let Ok(at) = files.binary_search_by(|(name, ..)| (*name).cmp(primary.file.as_str()))
    {
        files.swap(0, at);
    }
    for (name, file, annotations) in files {
        if let Some(snippet) = annotated_snippet(name, file, annotations) {
            group = group.element(snippet);
        }
    }
    group
}

/// rustc's `collect_annotations` of `MultiSpan::span_labels`: the labelled spans in the order
/// they were labelled, then the primary spans with no label, by file in the order they are
/// met, each file's by line, leaving out those not in their file.
fn collect_annotations<'a>(span: &'a MultiSpan, extra: Labels<'a>, placed: &Placed<'a>) -> Annotated<'a> {
    let unlabelled: SmallVec<[&'a Span; 2]> = span
        .primary
        .iter()
        .filter(|it| !span.labels.iter().any(|(at, _)| at == *it) && !extra.iter().any(|(at, _)| at == it))
        .collect();
    let labels = span
        .labels
        .iter()
        .map(|(at, label)| (at, Some(Cow::Borrowed(label.as_str()))))
        .chain(extra.into_iter().map(|(at, label)| (at, Some(label))))
        .chain(unlabelled.into_iter().map(|at| (at, None)));
    let mut output: Annotated<'a> = SmallVec::new();
    for (at, label) in labels {
        let file = placed.file_of(at);
        if !file.holds(at) {
            continue;
        }
        let lo = file.norm(at.lo);
        let is_primary = label.is_none() || span.primary.contains(at);
        let annotation = Annotation {
            kind: if is_primary { AnnotationKind::Primary } else { AnnotationKind::Context },
            lo,
            hi: file.norm(at.hi),
            line: file.line_of(lo),
            label: label.map(normalize_whitespace),
        };
        match output.iter_mut().find(|(name, ..)| *name == at.file) {
            Some((.., annotations)) => annotations.push(annotation),
            None => output.push((at.file.as_str(), file, smallvec![annotation])),
        }
    }
    for (.., annotations) in &mut output {
        radix::sort_by_key(annotations, |it| it.line);
    }
    output
}

fn annotated_snippet<'a>(
    name: &'a str,
    file: &'a File,
    annotations: SmallVec<[Annotation<'a>; 4]>,
) -> Option<Snippet<'a, annotate_snippets::Annotation<'a>>> {
    let (start, end, line) = shrink_file(file, annotations.iter().map(|it| (it.lo, it.hi)))?;
    Some(Snippet::source(&file.text[start..end]).line_start(line).path(name).annotations(
        annotations.into_iter().map(move |it| {
            let annotation = it.kind.span(it.lo.saturating_sub(start)..it.hi.saturating_sub(start));
            match it.label {
                Some(label) => annotation.label(label),
                None => annotation,
            }
        }),
    ))
}

/// rustc's `shrink_file`: where the whole lines `ranges` are on start and end, and the line
/// number of the first.
fn shrink_file(file: &File, ranges: impl Iterator<Item = (usize, usize)>) -> Option<(usize, usize, usize)> {
    let (lo, hi) = ranges.reduce(|(lo, hi), (a, b)| (lo.min(a), hi.max(b)))?;
    let lo_line = file.line_of(lo);
    let start = file.line_bounds(lo_line).start;
    let end = file.line_bounds(file.line_of(hi)).end;
    Some((start, end, lo_line + 1))
}

/// A part of a way to substitute, placed in its file's text.
struct Part<'a> {
    lo: usize,
    hi: usize,
    snippet: Cow<'a, str>,
}

/// The ways to substitute of a suggestion that rustc shows: those whose parts are all in one
/// file it holds them in, each with its parts in order, leaving out the parts that change
/// nothing, and the ways left with no part.
fn substitutions<'a>(
    substitutions: &'a [Vec<(Span, String)>],
    placed: &Placed<'a>,
) -> Vec<(&'a str, &'a File, SmallVec<[Part<'a>; 2]>)> {
    let mut out = Vec::with_capacity(substitutions.len());
    for parts in substitutions {
        let Some((first, _)) = parts.first() else { continue };
        let name = first.file.as_str();
        if parts.iter().any(|(span, _)| span.file != name) {
            continue;
        }
        let Some(file) = placed.get(name) else { continue };
        if parts.iter().any(|(span, _)| !file.holds(span)) {
            continue;
        }
        let mut kept: SmallVec<[Part<'a>; 2]> = parts
            .iter()
            .map(|(span, snippet)| Part {
                lo: file.norm(span.lo),
                hi: file.norm(span.hi),
                snippet: Cow::Borrowed(snippet.as_str()),
            })
            .collect();
        radix::sort_by_key(&mut kept, |it| it.lo);
        kept.retain(|it| is_different(file, &it.snippet, it.lo, it.hi));
        if !kept.is_empty() {
            out.push((name, file, kept));
        }
    }
    out
}

/// rustc's attribute special case: a suggestion that adds `#[...]` on a line of its own is
/// moved to the start of the line, with its indentation, and shown with the line after.
fn attribute_suggestion(file: &File, parts: &mut [Part<'_>]) -> bool {
    let [part] = parts else { return false };
    let trimmed = part.snippet.trim();
    if !(trimmed.starts_with("#[") && trimmed.ends_with(']') && part.snippet.ends_with('\n')) {
        return false;
    }
    if part.lo != part.hi {
        return false;
    }
    let before = &file.text[..part.lo];
    let before = before.rsplit_once('\n').map_or(before, |(_, it)| it);
    if !before.trim().is_empty() {
        return false;
    }
    if !before.is_empty() && !part.snippet.ends_with(before) {
        part.snippet.to_mut().insert_str(0, before);
        part.lo -= before.len();
        part.hi = part.lo;
    }
    true
}

/// rustc's `Span::macro_backtrace`: the expansions `span` is in, innermost first, leaving out
/// a macro's recursive calls of itself.
fn backtrace(span: &Span) -> impl Iterator<Item = &Expansion> {
    let mut prev: Option<&Span> = None;
    let mut at = span;
    std::iter::from_fn(move || {
        loop {
            let expansion = at.expansion.as_deref()?;
            let recursive = prev.is_some_and(|it| same_place(&expansion.call_site, it));
            prev = Some(at);
            at = &expansion.call_site;
            if !recursive {
                return Some(expansion);
            }
        }
    })
}

/// rustc's `Span::source_equal`.
fn same_place(a: &Span, b: &Span) -> bool {
    a.file == b.file && a.lo == b.lo && a.hi == b.hi
}

/// rustc's `render_multispan_macro_backtrace` without `-Z macro-backtrace`: a primary span in a
/// macro's expansion that is not in its call gets a label on the outermost call of a macro
/// whose definition rustc knows.
fn macro_labels<'a>(span: &'a MultiSpan, labels: &mut Labels<'a>) {
    for primary in &span.primary {
        let Some(expansion) = backtrace(primary).filter(|it| it.def_site.is_some()).last() else {
            continue;
        };
        let call = &expansion.call_site;
        if call.file == primary.file && call.lo <= primary.lo && primary.hi <= call.hi {
            continue;
        }
        let label = match expansion.kind {
            MacroKind::Attr => "in this attribute macro expansion",
            MacroKind::Derive => "in this derive macro expansion",
            MacroKind::Bang => "in this macro invocation",
        };
        if !labels.iter().any(|(at, it)| *at == call && *it == label) {
            labels.push((call, Cow::Borrowed(label)));
        }
    }
}

/// rustc's note on a diagnostic in a macro's expansion, which its emitter adds: the macro it
/// originates in, from the spans of it and of its children
/// (`fix_multispans_in_extern_macros_and_render_macro_backtrace`).
fn originates_in(diag: &Diag) -> Option<String> {
    let spans = diag.span.primary.iter().chain(diag.children.iter().flat_map(|it| &it.span.primary));
    let mut macros = spans.flat_map(backtrace);
    let first = macros.next()?;
    let last = macros.last().unwrap_or(first);
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
fn primary_span_formatted<'a>(
    suggestions: &'a [Suggestion],
    placed: &Placed<'a>,
) -> Option<(&'a Span, Cow<'a, str>)> {
    let [suggestion] = suggestions else { return None };
    let [substitution] = &suggestion.substitutions[..] else { return None };
    let [(part_span, snippet)] = &substitution[..] else { return None };
    let inline = suggestion.message.split_whitespace().count() < 10
        && !snippet.contains('\n')
        && !matches!(
            suggestion.style,
            SuggestionStyle::HideCodeAlways
                | SuggestionStyle::CompletelyHidden
                | SuggestionStyle::ShowAlways
        );
    if !inline {
        return None;
    }
    let snippet = snippet.trim();
    let label = if snippet.is_empty() || suggestion.style.hide_inline() {
        format!("help: {}", suggestion.message)
    } else {
        let confusion = match placed.get(&part_span.file).and_then(|it| it.snippet(part_span)) {
            Some(found) => confusion_of(found, snippet),
            None => Confusion::None,
        };
        format!("help: {}{}: `{snippet}`", suggestion.message, confusion.label_text())
    };
    Some((part_span, Cow::Owned(label)))
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

/// rustc's `normalize_whitespace`, borrowing a text that has none of the chars it replaces:
/// each is below U+0020, U+007F, or three bytes starting with `0xE2`, found by a scan of the
/// bytes that vectorizes.
fn normalize_whitespace(text: Cow<'_, str>) -> Cow<'_, str> {
    if !text.bytes().any(|b| b < 0x20 || b == 0x7f || b == 0xe2) {
        return text;
    }
    Cow::Owned(text.chars().fold(String::with_capacity(text.len()), |mut out, c| {
        match OUTPUT_REPLACEMENTS.binary_search_by_key(&c, |(it, _)| *it) {
            Ok(at) => out.push_str(OUTPUT_REPLACEMENTS[at].1),
            Err(_) => out.push(c),
        }
        out
    }))
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
    fn a_file_without_a_bom_or_crs_is_not_copied() {
        let read = "fn f() {}\n".to_owned();
        let at = read.as_ptr();
        let file = File::new(read);
        assert_eq!(file.text.as_ptr(), at);
        assert!(file.ascii);
        assert!(file.crs.is_empty());
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
        let reads = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
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

    #[test]
    fn text_without_chars_to_replace_is_borrowed() {
        assert!(matches!(normalize_whitespace(Cow::Borrowed("expected `u32`, 中文")), Cow::Borrowed(_)));
        assert_eq!(normalize_whitespace(Cow::Borrowed("a\tb\u{200d}c")), "a    bc");
        // `’` starts with 0xE2 too and is kept as it is.
        assert_eq!(normalize_whitespace(Cow::Borrowed("it’s")), "it’s");
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
        let names: Vec<&str> = backtrace(&at).map(|it| it.name.as_str()).collect();
        assert_eq!(names, ["inner", "outer"]);
    }

    #[test]
    fn a_call_where_the_span_two_steps_back_is_is_left_out_of_the_backtrace() {
        let root = Span { file: "a.rs".to_owned(), lo: 3, hi: 4, expansion: None };
        let at = expanded(3, 4, "m", expanded(20, 30, "m", root));
        let names: Vec<&str> = backtrace(&at).map(|it| it.name.as_str()).collect();
        assert_eq!(names, ["m"]);
    }
}
