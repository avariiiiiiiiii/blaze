//! cargo's JSON messages (`util/machine_message.rs`) and the rustc diagnostic each
//! `compiler-message` carries (`rustc_errors/src/json.rs`), byte for byte: fields in the order
//! cargo and rustc write them, `null` where they write `null`, strings escaped as serde_json
//! escapes them. Built as an ordered tree, never through a map whose order a feature of
//! serde_json could change.

use std::{
    collections::HashMap,
    path::{Path, PathBuf},
};

use blaze_api::{Artifact, BuildScriptRan, Diagnostic, Level, Span, UnitOf, UnitTarget};

use super::say;

/// A JSON value with its object fields in the order they are written.
#[derive(Clone, PartialEq, Eq, Debug)]
pub enum J {
    Null,
    Bool(bool),
    Num(u64),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(&'static str, J)>),
}

impl J {
    pub fn str(text: &str) -> J {
        J::Str(text.to_owned())
    }

    fn strs(texts: &[String]) -> J {
        J::Arr(texts.iter().map(|it| J::str(it)).collect())
    }

    fn opt(text: Option<&str>) -> J {
        text.map_or(J::Null, J::str)
    }

    /// As one line, without a newline.
    pub fn line(&self) -> String {
        let mut out = String::new();
        self.write(&mut out);
        out
    }

    fn write(&self, out: &mut String) {
        match self {
            J::Null => out.push_str("null"),
            J::Bool(it) => out.push_str(if *it { "true" } else { "false" }),
            J::Num(it) => out.push_str(&it.to_string()),
            J::Str(it) => quote(it, out),
            J::Arr(items) => {
                out.push('[');
                for (at, item) in items.iter().enumerate() {
                    if at > 0 {
                        out.push(',');
                    }
                    item.write(out);
                }
                out.push(']');
            }
            J::Obj(fields) => {
                out.push('{');
                for (at, (key, value)) in fields.iter().enumerate() {
                    if at > 0 {
                        out.push(',');
                    }
                    quote(key, out);
                    out.push(':');
                    value.write(out);
                }
                out.push('}');
            }
        }
    }

    /// Every object's fields sorted by key, as a `serde_json::Value` without
    /// `preserve_order` holds them.
    fn sorted(self) -> J {
        match self {
            J::Arr(items) => J::Arr(items.into_iter().map(J::sorted).collect()),
            J::Obj(fields) => {
                let mut fields: Vec<(&'static str, J)> =
                    fields.into_iter().map(|(key, value)| (key, value.sorted())).collect();
                fields.sort_by(|a, b| a.0.cmp(b.0));
                J::Obj(fields)
            }
            other => other,
        }
    }
}

/// `text` as serde_json writes a string.
fn quote(text: &str, out: &mut String) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push('"');
    for c in text.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => {
                let byte = c as u8;
                out.push_str("\\u00");
                out.push(HEX[usize::from(byte >> 4)] as char);
                out.push(HEX[usize::from(byte & 0xf)] as char);
            }
            c => out.push(c),
        }
    }
    out.push('"');
}

/// cargo's `Message::to_json_string`: the reason first, then the message's fields.
fn message(reason: &str, mut fields: Vec<(&'static str, J)>) -> String {
    fields.insert(0, ("reason", J::str(reason)));
    J::Obj(fields).line()
}

/// A target as cargo's `SerializedTarget` writes it. `required-features` is written only for a
/// target that has some, which [`UnitTarget`] does not say.
pub fn target(target: &UnitTarget) -> J {
    J::Obj(vec![
        ("kind", J::strs(&target.kind)),
        ("crate_types", J::strs(&target.crate_types)),
        ("name", J::str(&target.name)),
        ("src_path", J::str(&target.src_path)),
        ("edition", J::str(&target.edition)),
        ("doc", J::Bool(target.doc)),
        ("doctest", J::Bool(target.doctest)),
        ("test", J::Bool(target.test)),
    ])
}

/// `compiler-artifact`.
pub fn artifact(artifact: &Artifact) -> String {
    let profile = &artifact.profile;
    message("compiler-artifact", vec![
        ("package_id", J::str(&artifact.package)),
        ("manifest_path", J::str(&artifact.manifest_path)),
        ("target", target(&artifact.target)),
        (
            "profile",
            J::Obj(vec![
                ("opt_level", J::str(&profile.opt_level)),
                ("debuginfo", J::Num(u64::from(profile.debuginfo))),
                ("debug_assertions", J::Bool(profile.debug_assertions)),
                ("overflow_checks", J::Bool(profile.overflow_checks)),
                ("test", J::Bool(profile.test)),
            ]),
        ),
        ("features", J::strs(&artifact.features)),
        ("filenames", J::strs(&artifact.filenames)),
        ("executable", J::opt(artifact.executable.as_deref())),
        ("fresh", J::Bool(artifact.fresh)),
    ])
}

/// `build-script-executed`.
pub fn build_script(ran: &BuildScriptRan) -> String {
    let env = ran.env.iter().map(|(name, value)| J::Arr(vec![J::str(name), J::str(value)]));
    message("build-script-executed", vec![
        ("package_id", J::str(&ran.package_id)),
        ("linked_libs", J::strs(&ran.linked_libs)),
        ("linked_paths", J::strs(&ran.linked_paths)),
        ("cfgs", J::strs(&ran.cfgs)),
        ("env", J::Arr(env.collect())),
        ("out_dir", J::str(&ran.out_dir)),
    ])
}

/// `build-finished`.
pub fn build_finished(success: bool) -> String {
    message("build-finished", vec![("success", J::Bool(success))])
}

/// `compiler-message` of `unit`. With `ansi` (`json-diagnostic-rendered-ansi`) rustc's message
/// passes as rustc wrote it; without, cargo takes the colors out of `rendered` and writes the
/// message again: `rendered` first, then every other field sorted, at every depth.
pub fn compiler_message(unit: &UnitOf, diagnostic: &Diagnostic, ansi: bool, sources: &mut Sources) -> String {
    let rustc = rustc_diagnostic(diagnostic, true, sources, Path::new(&unit.manifest_path));
    let message = match ansi {
        true => rustc,
        false => recolored(rustc),
    };
    message_of(unit, message)
}

fn message_of(unit: &UnitOf, message: J) -> String {
    message("compiler-message", vec![
        ("package_id", J::str(&unit.package_id)),
        ("manifest_path", J::str(&unit.manifest_path)),
        ("target", target(&unit.target)),
        ("message", message),
    ])
}

/// cargo's `CompilerMessage { rendered, #[serde(flatten)] other: BTreeMap<_, Value> }`.
fn recolored(rustc: J) -> J {
    let J::Obj(fields) = rustc else { return rustc };
    let mut rendered = J::Null;
    let mut other = Vec::with_capacity(fields.len());
    for (key, value) in fields {
        match (key, value) {
            ("rendered", J::Str(text)) => rendered = J::Str(say::strip_ansi(&text)),
            ("rendered", value) => rendered = value,
            (key, value) => other.push((key, value)),
        }
    }
    let mut fields: Vec<(&'static str, J)> =
        other.into_iter().map(|(key, value)| (key, value.sorted())).collect();
    fields.sort_by(|a, b| a.0.cmp(b.0));
    fields.insert(0, ("rendered", rendered));
    J::Obj(fields)
}

fn level(level: Level) -> &'static str {
    match level {
        Level::Error => "error",
        Level::Warning => "warning",
        Level::Note => "note",
        Level::Help => "help",
        Level::FailureNote => "failure-note",
    }
}

/// rustc's `Diagnostic`: `$message_type` on the outermost only, `rendered` `null` on children.
fn rustc_diagnostic(diagnostic: &Diagnostic, outermost: bool, sources: &mut Sources, manifest: &Path) -> J {
    let mut fields = Vec::with_capacity(7);
    if outermost {
        fields.push(("$message_type", J::str("diagnostic")));
    }
    fields.push(("message", J::str(&diagnostic.message)));
    let code = diagnostic
        .code
        .as_deref()
        .map_or(J::Null, |code| J::Obj(vec![("code", J::str(code)), ("explanation", J::Null)]));
    fields.push(("code", code));
    fields.push(("level", J::str(level(diagnostic.level))));
    let spans = diagnostic.spans.iter().map(|span| rustc_span(span, sources, manifest));
    fields.push(("spans", J::Arr(spans.collect())));
    let children = diagnostic.children.iter().map(|it| rustc_diagnostic(it, false, sources, manifest));
    fields.push(("children", J::Arr(children.collect())));
    let rendered = match outermost {
        true => J::str(&diagnostic.rendered),
        false => J::Null,
    };
    fields.push(("rendered", rendered));
    J::Obj(fields)
}

/// rustc's `DiagnosticSpan`. `expansion` is `null`: [`Span`] carries no macro backtrace.
fn rustc_span(span: &Span, sources: &mut Sources, manifest: &Path) -> J {
    let text = sources.lines(&span.file, manifest).map_or_else(Vec::new, |lines| span_text(span, lines));
    J::Obj(vec![
        ("file_name", J::str(&span.file)),
        ("byte_start", J::Num(u64::from(span.byte_start))),
        ("byte_end", J::Num(u64::from(span.byte_end))),
        ("line_start", J::Num(u64::from(span.line))),
        ("line_end", J::Num(u64::from(span.end_line))),
        ("column_start", J::Num(u64::from(span.column))),
        ("column_end", J::Num(u64::from(span.end_column))),
        ("is_primary", J::Bool(span.primary)),
        ("text", J::Arr(text)),
        ("label", J::opt(span.label.as_deref())),
        ("suggested_replacement", J::opt(span.replacement.as_deref())),
        ("suggestion_applicability", J::opt(span.applicability.as_deref())),
        ("expansion", J::Null),
    ])
}

/// rustc's `DiagnosticSpanLine`s of `span` (`SourceMap::span_to_lines`): each line it covers,
/// highlighted from its start column on the first and to its end column on the last, in
/// 1-based columns of characters. None when the span is outside the file as it is on disk.
fn span_text(span: &Span, lines: &[String]) -> Vec<J> {
    let (first, last) = (span.line as usize, span.end_line as usize);
    if first == 0 || last < first || last > lines.len() {
        return Vec::new();
    }
    (first..=last)
        .map(|number| {
            let text = &lines[number - 1];
            let start = if number == first { u64::from(span.column) } else { 1 };
            let end = match number == last {
                true => u64::from(span.end_column),
                false => text.chars().count() as u64 + 1,
            };
            J::Obj(vec![
                ("text", J::str(text)),
                ("highlight_start", J::Num(start)),
                ("highlight_end", J::Num(end)),
            ])
        })
        .collect()
}

/// The source files spans point into, as on disk, each read once: rustc's JSON carries the
/// text of the lines a span covers.
pub struct Sources {
    root: PathBuf,
    read: HashMap<PathBuf, Option<Vec<String>>>,
}

impl Sources {
    /// `root` is the checkout's: a relative file name is looked for no higher.
    pub fn new(root: &Path) -> Sources {
        Sources { root: root.to_owned(), read: HashMap::new() }
    }

    /// The lines of `file` as rustc names it: absolute, or relative to the directory rustc ran
    /// in, which is the workspace root, an ancestor of the unit's `manifest` directory: the
    /// nearest one that has it. Lines as rustc keeps them: no BOM, no `\r` before a newline.
    pub fn lines(&mut self, file: &str, manifest: &Path) -> Option<&[String]> {
        let path = self.find(Path::new(file), manifest)?;
        let lines = self.read.entry(path.clone()).or_insert_with(|| {
            let bytes = std::fs::read(&path).ok()?;
            let text = String::from_utf8_lossy(&bytes);
            let text = text.strip_prefix('\u{feff}').unwrap_or(&text);
            Some(text.split('\n').map(|line| line.strip_suffix('\r').unwrap_or(line).to_owned()).collect())
        });
        lines.as_deref()
    }

    fn find(&self, file: &Path, manifest: &Path) -> Option<PathBuf> {
        if file.is_absolute() {
            return Some(file.to_owned());
        }
        let dir = manifest.parent()?;
        dir.ancestors()
            .take_while(|it| it.starts_with(&self.root))
            .map(|it| it.join(file))
            .find(|it| std::fs::metadata(it).is_ok_and(|it| it.is_file()))
    }
}

#[cfg(test)]
mod tests;
