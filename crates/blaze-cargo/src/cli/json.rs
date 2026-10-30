//! cargo's JSON messages (`util/machine_message.rs`) and the rustc diagnostic each
//! `compiler-message` carries (`rustc_errors/src/json.rs`), byte for byte: fields in the order
//! cargo and rustc write them, `null` where they write `null`, strings escaped as serde_json
//! escapes them. Each message is written straight into the caller's buffer, which it reuses:
//! no tree, no copy of a field, no allocation per message once the buffer has grown.

use std::hash::BuildHasher as _;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use blaze_api::{Artifact, BuildScriptRan, Diagnostic, Level, Span, UnitOf, UnitTarget};
use rustc_hash::{FxBuildHasher, FxHashMap};

const HEX: &[u8; 16] = b"0123456789abcdef";

/// serde_json's escape table: 0 for a byte written as it is, else the letter after the `\`
/// (`u` for `\u00XX`). ESC is among the escaped, which is where cargo's stripping of colors
/// joins the escape.
const ESCAPE: [u8; 256] = {
    let mut table = [0u8; 256];
    let mut byte = 0;
    while byte < 0x20 {
        table[byte] = b'u';
        byte += 1;
    }
    table[0x08] = b'b';
    table[0x09] = b't';
    table[0x0a] = b'n';
    table[0x0c] = b'f';
    table[0x0d] = b'r';
    table[b'"' as usize] = b'"';
    table[b'\\' as usize] = b'\\';
    table
};

/// `text` as serde_json writes a string: runs of bytes that need no escape are copied whole.
pub fn quote(out: &mut String, text: &str) {
    quote_with(out, text, false);
}

/// [`quote`], with the escapes that color a terminal taken out when `strip`, as
/// `anstream::adapter::strip_str` takes them out ([`super::say::strip_ansi`]): CSI sequences to
/// their final byte, OSC sequences to BEL or ST, any other escape with the character after it.
fn quote_with(out: &mut String, text: &str, strip: bool) {
    let bytes = text.as_bytes();
    out.reserve(bytes.len() + 2);
    out.push('"');
    let (mut run, mut at) = (0, 0);
    while at < bytes.len() {
        let escape = ESCAPE[usize::from(bytes[at])];
        if escape == 0 {
            at += 1;
            continue;
        }
        out.push_str(&text[run..at]);
        if strip && bytes[at] == 0x1b {
            at = after_escape(text, at);
        } else {
            match escape {
                b'u' => {
                    out.push_str("\\u00");
                    out.push(char::from(HEX[usize::from(bytes[at] >> 4)]));
                    out.push(char::from(HEX[usize::from(bytes[at] & 0xf)]));
                }
                letter => {
                    out.push('\\');
                    out.push(char::from(letter));
                }
            }
            at += 1;
        }
        run = at;
    }
    out.push_str(&text[run..]);
    out.push('"');
}

/// Where the terminal escape at `at` ends; always on a character boundary, since every byte
/// that ends one is ASCII.
fn after_escape(text: &str, at: usize) -> usize {
    let bytes = text.as_bytes();
    match bytes.get(at + 1) {
        Some(b'[') => bytes[at + 2..]
            .iter()
            .position(|it| (0x40..=0x7e).contains(it))
            .map_or(bytes.len(), |end| at + 2 + end + 1),
        Some(b']') => {
            let mut next = at + 2;
            while next < bytes.len() {
                match bytes[next] {
                    0x07 => return next + 1,
                    0x1b if bytes.get(next + 1) == Some(&b'\\') => return next + 2,
                    _ => next += 1,
                }
            }
            bytes.len()
        }
        Some(_) => at + 1 + text[at + 1..].chars().next().map_or(0, char::len_utf8),
        None => bytes.len(),
    }
}

fn num(out: &mut String, mut n: u64) {
    let mut digits = [0u8; 20];
    let mut at = digits.len();
    loop {
        at -= 1;
        digits[at] = b'0' + (n % 10) as u8;
        n /= 10;
        if n == 0 {
            break;
        }
    }
    out.extend(digits[at..].iter().map(|&it| char::from(it)));
}

fn boolean(out: &mut String, value: bool) {
    out.push_str(if value { "true" } else { "false" });
}

fn opt(out: &mut String, text: Option<&str>) {
    match text {
        Some(text) => quote(out, text),
        None => out.push_str("null"),
    }
}

fn strs(out: &mut String, texts: &[String]) {
    out.push('[');
    for (at, text) in texts.iter().enumerate() {
        if at > 0 {
            out.push(',');
        }
        quote(out, text);
    }
    out.push(']');
}

/// A target as cargo's `SerializedTarget` writes it. `required-features` is written only for a
/// target that has some, which [`UnitTarget`] does not say.
pub fn target(out: &mut String, target: &UnitTarget) {
    out.push_str(r#"{"kind":"#);
    strs(out, &target.kind);
    out.push_str(r#","crate_types":"#);
    strs(out, &target.crate_types);
    out.push_str(r#","name":"#);
    quote(out, &target.name);
    out.push_str(r#","src_path":"#);
    quote(out, &target.src_path);
    out.push_str(r#","edition":"#);
    quote(out, &target.edition);
    out.push_str(r#","doc":"#);
    boolean(out, target.doc);
    out.push_str(r#","doctest":"#);
    boolean(out, target.doctest);
    out.push_str(r#","test":"#);
    boolean(out, target.test);
    out.push('}');
}

/// `compiler-artifact`, without its newline.
pub fn artifact(out: &mut String, artifact: &Artifact) {
    let profile = &artifact.profile;
    out.push_str(r#"{"reason":"compiler-artifact","package_id":"#);
    quote(out, &artifact.package);
    out.push_str(r#","manifest_path":"#);
    quote(out, &artifact.manifest_path);
    out.push_str(r#","target":"#);
    target(out, &artifact.target);
    out.push_str(r#","profile":{"opt_level":"#);
    quote(out, &profile.opt_level);
    out.push_str(r#","debuginfo":"#);
    num(out, u64::from(profile.debuginfo));
    out.push_str(r#","debug_assertions":"#);
    boolean(out, profile.debug_assertions);
    out.push_str(r#","overflow_checks":"#);
    boolean(out, profile.overflow_checks);
    out.push_str(r#","test":"#);
    boolean(out, profile.test);
    out.push_str(r#"},"features":"#);
    strs(out, &artifact.features);
    out.push_str(r#","filenames":"#);
    strs(out, &artifact.filenames);
    out.push_str(r#","executable":"#);
    opt(out, artifact.executable.as_deref());
    out.push_str(r#","fresh":"#);
    boolean(out, artifact.fresh);
    out.push('}');
}

/// `build-script-executed`, without its newline.
pub fn build_script(out: &mut String, ran: &BuildScriptRan) {
    out.push_str(r#"{"reason":"build-script-executed","package_id":"#);
    quote(out, &ran.package_id);
    out.push_str(r#","linked_libs":"#);
    strs(out, &ran.linked_libs);
    out.push_str(r#","linked_paths":"#);
    strs(out, &ran.linked_paths);
    out.push_str(r#","cfgs":"#);
    strs(out, &ran.cfgs);
    out.push_str(r#","env":["#);
    for (at, (name, value)) in ran.env.iter().enumerate() {
        if at > 0 {
            out.push(',');
        }
        out.push('[');
        quote(out, name);
        out.push(',');
        quote(out, value);
        out.push(']');
    }
    out.push_str(r#"],"out_dir":"#);
    quote(out, &ran.out_dir);
    out.push('}');
}

/// `build-finished`, without its newline.
pub fn build_finished(out: &mut String, success: bool) {
    out.push_str(r#"{"reason":"build-finished","success":"#);
    boolean(out, success);
    out.push('}');
}

/// `compiler-message` of `unit`, without its newline. With `ansi`
/// (`json-diagnostic-rendered-ansi`) rustc's message passes as rustc wrote it; without, cargo
/// takes the colors out of `rendered` and writes the message again through
/// `CompilerMessage { rendered, #[serde(flatten)] other: BTreeMap, code: Option<DiagnosticCode> }`:
/// `rendered` first, then every other field sorted by key (at every depth, as `serde_json::Value`
/// maps are), then the outermost `code` last.
pub fn compiler_message(out: &mut String, unit: &UnitOf, diagnostic: &Diagnostic, ansi: bool, sources: &mut Sources) {
    let dir = unit.manifest_path.rsplit_once(['/', '\\']).map_or("", |(dir, _)| dir);
    out.push_str(r#"{"reason":"compiler-message","package_id":"#);
    quote(out, &unit.package_id);
    out.push_str(r#","manifest_path":"#);
    quote(out, &unit.manifest_path);
    out.push_str(r#","target":"#);
    target(out, &unit.target);
    out.push_str(r#","message":"#);
    let order = if ansi { Order::Rustc } else { Order::Sorted };
    diagnostic_json(out, diagnostic, true, order, sources, dir);
    out.push('}');
}

/// The field order a diagnostic is written in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Order {
    /// rustc's, as its `Serialize` impls declare the fields.
    Rustc,
    /// serde_json's `Map` without `preserve_order`: by key.
    Sorted,
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

fn code(out: &mut String, code: Option<&str>) {
    match code {
        Some(code) => {
            out.push_str(r#"{"code":"#);
            quote(out, code);
            out.push_str(r#","explanation":null}"#);
        }
        None => out.push_str("null"),
    }
}

/// rustc's `Diagnostic`: `$message_type` on the outermost only, `rendered` `null` on children.
fn diagnostic_json(out: &mut String, diagnostic: &Diagnostic, outermost: bool, order: Order, sources: &mut Sources, dir: &str) {
    let spans = |out: &mut String, sources: &mut Sources| {
        out.push('[');
        for (at, span) in diagnostic.spans.iter().enumerate() {
            if at > 0 {
                out.push(',');
            }
            span_json(out, span, order, sources, dir);
        }
        out.push(']');
    };
    let children = |out: &mut String, sources: &mut Sources| {
        out.push('[');
        for (at, child) in diagnostic.children.iter().enumerate() {
            if at > 0 {
                out.push(',');
            }
            diagnostic_json(out, child, false, order, sources, dir);
        }
        out.push(']');
    };
    match (order, outermost) {
        (Order::Rustc, _) => {
            out.push('{');
            if outermost {
                out.push_str(r#""$message_type":"diagnostic","#);
            }
            out.push_str(r#""message":"#);
            quote(out, &diagnostic.message);
            out.push_str(r#","code":"#);
            code(out, diagnostic.code.as_deref());
            out.push_str(r#","level":""#);
            out.push_str(level(diagnostic.level));
            out.push_str(r#"","spans":"#);
            spans(out, sources);
            out.push_str(r#","children":"#);
            children(out, sources);
            out.push_str(r#","rendered":"#);
            match outermost {
                true => quote(out, &diagnostic.rendered),
                false => out.push_str("null"),
            }
            out.push('}');
        }
        (Order::Sorted, true) => {
            out.push_str(r#"{"rendered":"#);
            quote_with(out, &diagnostic.rendered, true);
            out.push_str(r#","$message_type":"diagnostic","children":"#);
            children(out, sources);
            out.push_str(r#","level":""#);
            out.push_str(level(diagnostic.level));
            out.push_str(r#"","message":"#);
            quote(out, &diagnostic.message);
            out.push_str(r#","spans":"#);
            spans(out, sources);
            out.push_str(r#","code":"#);
            code(out, diagnostic.code.as_deref());
            out.push('}');
        }
        (Order::Sorted, false) => {
            out.push_str(r#"{"children":"#);
            children(out, sources);
            out.push_str(r#","code":"#);
            code(out, diagnostic.code.as_deref());
            out.push_str(r#","level":""#);
            out.push_str(level(diagnostic.level));
            out.push_str(r#"","message":"#);
            quote(out, &diagnostic.message);
            out.push_str(r#","rendered":null,"spans":"#);
            spans(out, sources);
            out.push('}');
        }
    }
}

/// rustc's `DiagnosticSpan`. `expansion` is `null`: [`Span`] carries no macro backtrace.
fn span_json(out: &mut String, span: &Span, order: Order, sources: &mut Sources, dir: &str) {
    let file = sources.file(&span.file, dir);
    match order {
        Order::Rustc => {
            out.push_str(r#"{"file_name":"#);
            quote(out, &span.file);
            out.push_str(r#","byte_start":"#);
            num(out, u64::from(span.byte_start));
            out.push_str(r#","byte_end":"#);
            num(out, u64::from(span.byte_end));
            out.push_str(r#","line_start":"#);
            num(out, u64::from(span.line));
            out.push_str(r#","line_end":"#);
            num(out, u64::from(span.end_line));
            out.push_str(r#","column_start":"#);
            num(out, u64::from(span.column));
            out.push_str(r#","column_end":"#);
            num(out, u64::from(span.end_column));
            out.push_str(r#","is_primary":"#);
            boolean(out, span.primary);
            out.push_str(r#","text":"#);
            span_text(out, span, order, sources, file);
            out.push_str(r#","label":"#);
            opt(out, span.label.as_deref());
            out.push_str(r#","suggested_replacement":"#);
            opt(out, span.replacement.as_deref());
            out.push_str(r#","suggestion_applicability":"#);
            opt(out, span.applicability.as_deref());
            out.push_str(r#","expansion":null}"#);
        }
        Order::Sorted => {
            out.push_str(r#"{"byte_end":"#);
            num(out, u64::from(span.byte_end));
            out.push_str(r#","byte_start":"#);
            num(out, u64::from(span.byte_start));
            out.push_str(r#","column_end":"#);
            num(out, u64::from(span.end_column));
            out.push_str(r#","column_start":"#);
            num(out, u64::from(span.column));
            out.push_str(r#","expansion":null,"file_name":"#);
            quote(out, &span.file);
            out.push_str(r#","is_primary":"#);
            boolean(out, span.primary);
            out.push_str(r#","label":"#);
            opt(out, span.label.as_deref());
            out.push_str(r#","line_end":"#);
            num(out, u64::from(span.end_line));
            out.push_str(r#","line_start":"#);
            num(out, u64::from(span.line));
            out.push_str(r#","suggested_replacement":"#);
            opt(out, span.replacement.as_deref());
            out.push_str(r#","suggestion_applicability":"#);
            opt(out, span.applicability.as_deref());
            out.push_str(r#","text":"#);
            span_text(out, span, order, sources, file);
            out.push('}');
        }
    }
}

/// rustc's `DiagnosticSpanLine`s of `span` (`SourceMap::span_to_lines`): each line it covers,
/// highlighted from its start column on the first and to its end column on the last, in
/// 1-based columns of characters. `[]` when the span is outside the file as it is on disk.
fn span_text(out: &mut String, span: &Span, order: Order, sources: &Sources, file: Option<u32>) {
    let (first, last) = (span.line as usize, span.end_line as usize);
    let lines = file.map_or(0, |file| sources.line_count(file));
    out.push('[');
    if let Some(file) = file
        && first != 0
        && last >= first
        && last <= lines
    {
        for number in first..=last {
            let text = sources.line(file, number);
            let start = if number == first { u64::from(span.column) } else { 1 };
            let end = match number == last {
                true => u64::from(span.end_column),
                false => text.chars().count() as u64 + 1,
            };
            if number > first {
                out.push(',');
            }
            match order {
                Order::Rustc => {
                    out.push_str(r#"{"text":"#);
                    quote(out, text);
                    out.push_str(r#","highlight_start":"#);
                    num(out, start);
                    out.push_str(r#","highlight_end":"#);
                    num(out, end);
                }
                Order::Sorted => {
                    out.push_str(r#"{"highlight_end":"#);
                    num(out, end);
                    out.push_str(r#","highlight_start":"#);
                    num(out, start);
                    out.push_str(r#","text":"#);
                    quote(out, text);
                }
            }
            out.push('}');
        }
    }
    out.push(']');
}

/// One source file as on disk, read once, as a range of [`Sources`]' text and line starts.
/// Lines as rustc keeps them: no BOM, no `\r` before a newline.
struct Source {
    /// Its first line's index in `Sources::starts`.
    first: u32,
    /// As many as `split('\n')` has lines.
    lines: u32,
    /// Its text's end in `Sources::text`.
    end: u32,
}

/// The source files spans point into, as on disk, each read once: rustc's JSON carries the
/// text of the lines a span covers. A name is looked up by the Fx hash of the directory of its
/// unit's manifest and the name as the span has it; every file's text goes into one buffer and
/// every line start into another, 4 bytes a line, so that once the buffers have grown a lookup
/// or a read allocates nothing.
pub struct Sources {
    root: PathBuf,
    /// Each (manifest directory, name)'s file, by hash; `None` for a name that is no file.
    named: FxHashMap<u64, Option<u32>>,
    /// Each file read, by the hash of its path, so that two names of one file read it once.
    at: FxHashMap<u64, u32>,
    files: Vec<Source>,
    text: String,
    starts: Vec<u32>,
    /// The path being looked for, and the bytes of the file being read.
    path: PathBuf,
    bytes: Vec<u8>,
}

const FILES: usize = 256;
const TEXT: usize = 1 << 20;

impl Sources {
    /// `root` is the checkout's: a relative file name is looked for no higher.
    pub fn new(root: &Path) -> Sources {
        Sources {
            // alloc: once a request.
            root: root.to_owned(),
            named: FxHashMap::with_capacity_and_hasher(FILES, FxBuildHasher),
            at: FxHashMap::with_capacity_and_hasher(FILES, FxBuildHasher),
            files: Vec::with_capacity(FILES),
            text: String::with_capacity(TEXT),
            starts: Vec::with_capacity(TEXT / 32),
            path: PathBuf::with_capacity(1024),
            bytes: Vec::with_capacity(TEXT / 4),
        }
    }

    /// The file `name` is, as rustc names it: absolute, or relative to the directory rustc ran
    /// in, which is the workspace root, an ancestor of the unit's manifest directory `dir`: the
    /// nearest one that has it.
    pub fn file(&mut self, name: &str, dir: &str) -> Option<u32> {
        let key = FxBuildHasher.hash_one((dir, name));
        if let Some(&found) = self.named.get(&key) {
            return found;
        }
        let found = match self.find(Path::new(name), Path::new(dir)) {
            true => self.read(),
            false => None,
        };
        self.named.insert(key, found);
        found
    }

    /// The file at `path`, read the first time.
    fn read(&mut self) -> Option<u32> {
        let key = FxBuildHasher.hash_one(&self.path);
        if let Some(&at) = self.at.get(&key) {
            return Some(at);
        }
        self.bytes.clear();
        std::fs::File::open(&self.path).and_then(|mut file| file.read_to_end(&mut self.bytes)).ok()?;
        let begin = self.text.len();
        match std::str::from_utf8(&self.bytes) {
            Ok(text) => self.text.push_str(text),
            // alloc: a file that is not UTF-8, as rustc reads it.
            Err(_) => self.text.push_str(&String::from_utf8_lossy(&self.bytes)),
        }
        let first = self.starts.len();
        let start = match self.text[begin..].starts_with('\u{feff}') {
            true => begin + '\u{feff}'.len_utf8(),
            false => begin,
        };
        self.starts.push(start as u32);
        let text = &self.text[start..];
        self.starts.extend(text.match_indices('\n').map(|(at, _)| (start + at + 1) as u32));
        let at = self.files.len() as u32;
        let lines = (self.starts.len() - first) as u32;
        self.files.push(Source { first: first as u32, lines, end: self.text.len() as u32 });
        self.at.insert(key, at);
        Some(at)
    }

    /// Puts where `file` is into `path`; false for nowhere.
    fn find(&mut self, file: &Path, dir: &Path) -> bool {
        self.path.clear();
        if file.is_absolute() {
            self.path.push(file);
            return true;
        }
        for it in dir.ancestors() {
            if !it.starts_with(&self.root) {
                break;
            }
            self.path.clear();
            self.path.push(it);
            self.path.push(file);
            if std::fs::metadata(&self.path).is_ok_and(|it| it.is_file()) {
                return true;
            }
        }
        false
    }

    pub fn line_count(&self, file: u32) -> usize {
        self.files[file as usize].lines as usize
    }

    /// Line `number` (1-based, at most [`Sources::line_count`]) of `file`.
    pub fn line(&self, file: u32, number: usize) -> &str {
        let source = &self.files[file as usize];
        let at = source.first as usize + number - 1;
        let start = self.starts[at] as usize;
        let end = match number < source.lines as usize {
            true => self.starts[at + 1] as usize - 1,
            false => source.end as usize,
        };
        let line = &self.text[start..end];
        line.strip_suffix('\r').unwrap_or(line)
    }
}

#[cfg(test)]
mod tests;
