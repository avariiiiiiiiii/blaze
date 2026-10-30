//! Text-based stubs (`.tbd`): the YAML Apple's `tapi` writes for the SDK's libraries, read
//! by hand for the subset it uses (versions 1 to 4; version 5 is JSON).
//!
//! The target is chosen per document as ld64's `tapi` reader does: the output's arch when
//! the document lists it on the output's platform, else the ABI-compatible one it lists
//! there (`arm64e` for `arm64`, `x86_64h` for `x86_64`), since an `arm64` process loads the
//! system's `arm64e` libraries (the SDK's `libSystem.B.tbd` lists no `arm64-macos` itself).

use blaze_codegen::Arch;
use rustc_hash::FxHashSet;

use crate::model::{Shared, SharedSymbol, SymbolKind};

/// A text-based stub (`.tbd`, as the macOS SDK ships `libSystem.tbd`) for `arch` on
/// `platform` (`LC_BUILD_VERSION`'s number: 1 macOS, 2 iOS, 3 tvOS, 4 watchOS, 6 Mac
/// Catalyst, 7 iOS simulator, 8 tvOS simulator, 9 watchOS simulator, 11 visionOS,
/// 12 visionOS simulator).
pub(super) fn read(name: &str, text: &str, arch: Arch, platform: u32) -> Result<Shared, String> {
    read_documents(text, arch, platform).map_err(|what| format!("{name}: {what}"))
}

fn read_documents(text: &str, arch: Arch, platform: u32) -> Result<Shared, String> {
    if matches!(arch, Arch::Wasm32) {
        return Err("a `.tbd` describes a Mach-O library; this output is wasm32".to_owned());
    }
    if text.trim_start().starts_with('{') {
        return Err("a JSON `.tbd` (version 5) is not read: only versions 1 to 4".to_owned());
    }
    let raw = documents(text)?;
    if raw.is_empty() {
        return Err("no document".to_owned());
    }
    let mut docs = Vec::with_capacity(raw.len());
    for (tag, lines) in &raw {
        docs.push(Doc::new(tag.as_deref(), lines)?);
    }

    let mut out = Exports::default();
    let mut seen = FxHashSet::default();
    seen.insert(docs[0].install_name.clone());
    fold(&docs, 0, arch, platform, &mut seen, &mut out)?;
    Ok(Shared {
        name: docs[0].install_name.clone(),
        symbols: out.symbols,
        more: Vec::new(),
        inputs: out.inputs,
    })
}

/// Adds document `at`'s exports to `out`, and those of every library it re-exports that is
/// a document of the file; the others' install names go to `out.inputs`.
fn fold(
    docs: &[Doc],
    at: usize,
    arch: Arch,
    platform: u32,
    seen: &mut FxHashSet<String>,
    out: &mut Exports,
) -> Result<(), String> {
    let doc = &docs[at];
    let chosen = doc.choose(arch, platform)?;
    let reexported = doc.exports(&chosen, platform, out)?;
    for library in reexported {
        if !seen.insert(library.clone()) {
            continue;
        }
        match docs.iter().position(|it| it.install_name == library) {
            Some(index) => fold(docs, index, arch, platform, seen, out)?,
            None => out.inputs.push(library),
        }
    }
    Ok(())
}

#[derive(Default)]
struct Exports {
    symbols: Vec<SharedSymbol>,
    names: FxHashSet<String>,
    inputs: Vec<String>,
}

impl Exports {
    fn add(&mut self, raw: &str, kind: SymbolKind, weak: bool) -> Result<(), String> {
        let name = super::core_name(raw.as_bytes())?;
        if self.names.insert(name.clone()) {
            self.symbols.push(SharedSymbol { name, version: None, kind, weak });
        }
        Ok(())
    }
}

/// A target a document or one of its lists names.
#[derive(Clone, PartialEq, Eq, Debug)]
struct Target {
    arch: String,
    /// `LC_BUILD_VERSION`'s number; 0 for a platform this reader doesn't know.
    platform: u32,
}

/// One document, its version known and its install name read.
struct Doc {
    version: u8,
    install_name: String,
    /// What the document lists, as written, for the error when the output's target is not
    /// among them.
    listed: Vec<String>,
    targets: Vec<Target>,
    map: Vec<(String, Value)>,
}

impl Doc {
    fn new(tag: Option<&str>, lines: &[(usize, &str)]) -> Result<Doc, String> {
        let first = lines.first().map_or(0, |it| it.0);
        let map = match parse_block(lines)? {
            Value::Map(map) => map,
            Value::Scalar(it) if it.is_empty() => Vec::new(),
            _ => return Err(format!("line {first}: a document must be a mapping")),
        };
        let version = match tag {
            None => 1,
            Some("!tapi-tbd-v2") => 2,
            Some("!tapi-tbd-v3") => 3,
            Some("!tapi-tbd") => {
                let version = get(&map, "tbd-version")
                    .ok_or_else(|| format!("line {first}: `!tapi-tbd` without `tbd-version`"))?;
                match scalar(version, "tbd-version")? {
                    "4" => 4,
                    other => {
                        return Err(format!(
                            "line {first}: tbd-version {other} is not read: only versions 1 to 4"
                        ));
                    }
                }
            }
            Some(other) => return Err(format!("line {first}: unknown document tag `{other}`")),
        };
        let install_name = get(&map, "install-name")
            .ok_or_else(|| format!("line {first}: a document without `install-name`"))
            .and_then(|it| scalar(it, "install-name"))?
            .to_owned();

        let mut listed = Vec::new();
        let mut targets = Vec::new();
        if version == 4 {
            let list = get(&map, "targets")
                .ok_or_else(|| format!("`{install_name}` has no `targets`"))?;
            for target in scalars(list, "targets")? {
                targets.push(v4_target(target)?);
                listed.push(target.to_owned());
            }
        } else {
            let archs = get(&map, "archs").ok_or_else(|| format!("`{install_name}` has no `archs`"))?;
            let platform = get(&map, "platform")
                .ok_or_else(|| format!("`{install_name}` has no `platform`"))
                .and_then(|it| scalar(it, "platform"))?;
            for arch in scalars(archs, "archs")? {
                for &number in v3_platforms(platform, arch) {
                    targets.push(Target { arch: arch.to_owned(), platform: number });
                }
                listed.push(format!("{arch} ({platform})"));
            }
        }
        Ok(Doc { version, install_name, listed, targets, map })
    }

    /// The arch this document is read for: `arch` itself when listed on `platform`, else the
    /// ABI-compatible arch listed there that ld64 would take.
    fn choose(&self, arch: Arch, platform: u32) -> Result<String, String> {
        self.targets
            .iter()
            .filter(|it| it.platform == platform)
            .filter_map(|it| arch_rank(arch, &it.arch).map(|rank| (rank, &it.arch)))
            .min_by_key(|(rank, _)| *rank)
            .map(|(_, it)| it.clone())
            .ok_or_else(|| {
                format!(
                    "`{}` has no {}-{}; it has {}",
                    self.install_name,
                    arch_name(arch),
                    platform_name(platform),
                    self.listed.join(", ")
                )
            })
    }

    /// Adds the exports of the lists for `chosen` to `out` and returns the libraries this one
    /// re-exports, in order.
    fn exports(&self, chosen: &str, platform: u32, out: &mut Exports) -> Result<Vec<String>, String> {
        let mut libraries = Vec::new();
        if self.version == 4 {
            for key in ["exports", "reexports"] {
                for section in self.sections(key)? {
                    if self.selects(section, "targets", chosen, platform)? {
                        self.section_exports(key, section, &mut libraries, out)?;
                    }
                }
            }
            for section in self.sections("reexported-libraries")? {
                if self.selects(section, "targets", chosen, platform)? {
                    for (key, value) in section {
                        match key.as_str() {
                            "targets" => {}
                            "libraries" => libraries
                                .extend(scalars(value, "libraries")?.into_iter().map(str::to_owned)),
                            other => return Err(self.unknown("reexported-libraries", other)),
                        }
                    }
                }
            }
        } else {
            for section in self.sections("exports")? {
                if self.selects(section, "archs", chosen, platform)? {
                    self.section_exports("exports", section, &mut libraries, out)?;
                }
            }
        }
        Ok(libraries)
    }

    fn section_exports(
        &self,
        list: &str,
        section: &[(String, Value)],
        libraries: &mut Vec<String>,
        out: &mut Exports,
    ) -> Result<(), String> {
        let v4 = self.version == 4;
        // Versions 1 and 2 write Objective-C classes and ivars with a leading `_`.
        let objc = |name: &str| -> String {
            match self.version {
                1 | 2 => name.strip_prefix('_').unwrap_or(name).to_owned(),
                _ => name.to_owned(),
            }
        };
        for (key, value) in section {
            match key.as_str() {
                "targets" if v4 => {}
                "archs" if !v4 => {}
                "allowable-clients" | "allowed-clients" => {}
                "re-exports" if !v4 => {
                    libraries.extend(scalars(value, key)?.into_iter().map(str::to_owned));
                }
                "symbols" => {
                    for name in scalars(value, key)? {
                        out.add(name, SymbolKind::Other, false)?;
                    }
                }
                "weak-symbols" if v4 => {
                    for name in scalars(value, key)? {
                        out.add(name, SymbolKind::Other, true)?;
                    }
                }
                "weak-def-symbols" if !v4 => {
                    for name in scalars(value, key)? {
                        out.add(name, SymbolKind::Other, true)?;
                    }
                }
                "thread-local-symbols" => {
                    for name in scalars(value, key)? {
                        out.add(name, SymbolKind::Tls, false)?;
                    }
                }
                "objc-classes" => {
                    for name in scalars(value, key)? {
                        let name = objc(name);
                        out.add(&format!("_OBJC_CLASS_$_{name}"), SymbolKind::Other, false)?;
                        out.add(&format!("_OBJC_METACLASS_$_{name}"), SymbolKind::Other, false)?;
                    }
                }
                "objc-eh-types" => {
                    for name in scalars(value, key)? {
                        out.add(&format!("_OBJC_EHTYPE_$_{name}"), SymbolKind::Other, false)?;
                    }
                }
                "objc-ivars" => {
                    for name in scalars(value, key)? {
                        let name = objc(name);
                        out.add(&format!("_OBJC_IVAR_$_{name}"), SymbolKind::Other, false)?;
                    }
                }
                other => return Err(self.unknown(list, other)),
            }
        }
        Ok(())
    }

    /// The mappings of the block sequence under `key`; none when the key is absent.
    fn sections(&self, key: &str) -> Result<Vec<&[(String, Value)]>, String> {
        let Some(value) = get(&self.map, key) else { return Ok(Vec::new()) };
        match value {
            Value::Seq(items) => items
                .iter()
                .map(|item| match item {
                    Value::Map(map) => Ok(map.as_slice()),
                    _ => Err(format!("`{}`: an entry of `{key}` is not a mapping", self.install_name)),
                })
                .collect(),
            Value::Scalar(it) if it.is_empty() => Ok(Vec::new()),
            _ => Err(format!("`{}`: `{key}` is not a sequence", self.install_name)),
        }
    }

    /// Whether a list is one this output reads: its `selector` (`targets` or `archs`) names
    /// `chosen` on `platform` (in v1 to v3 the document's platform, which `choose` checked).
    fn selects(
        &self,
        section: &[(String, Value)],
        selector: &str,
        chosen: &str,
        platform: u32,
    ) -> Result<bool, String> {
        let value = get(section, selector)
            .ok_or_else(|| format!("`{}`: a list without `{selector}`", self.install_name))?;
        let names = scalars(value, selector)?;
        if self.version == 4 {
            for name in names {
                let target = v4_target(name)?;
                if target.arch == chosen && target.platform == platform {
                    return Ok(true);
                }
            }
            Ok(false)
        } else {
            Ok(names.contains(&chosen))
        }
    }

    fn unknown(&self, list: &str, key: &str) -> String {
        format!("`{}`: unknown key `{key}` in `{list}`", self.install_name)
    }
}

fn arch_name(arch: Arch) -> &'static str {
    match arch {
        Arch::Aarch64 => "arm64",
        Arch::X86_64 => "x86_64",
        Arch::Wasm32 => "wasm32",
    }
}

/// How close a `.tbd` arch is to `arch`: 0 for `arch` itself, more for an ABI-compatible
/// one, `None` for one of another CPU.
fn arch_rank(arch: Arch, name: &str) -> Option<u8> {
    match arch {
        Arch::Aarch64 => match name {
            "arm64" => Some(0),
            "arm64e" => Some(1),
            _ if name.starts_with("arm64e.") => Some(2),
            _ => None,
        },
        Arch::X86_64 => match name {
            "x86_64" => Some(0),
            "x86_64h" => Some(1),
            _ => None,
        },
        Arch::Wasm32 => None,
    }
}

const PLATFORMS: [(&str, u32); 14] = [
    ("macos", 1),
    ("ios", 2),
    ("tvos", 3),
    ("watchos", 4),
    ("bridgeos", 5),
    ("maccatalyst", 6),
    ("ios-simulator", 7),
    ("tvos-simulator", 8),
    ("watchos-simulator", 9),
    ("driverkit", 10),
    ("xros", 11),
    ("xros-simulator", 12),
    ("visionos", 11),
    ("visionos-simulator", 12),
];

fn platform_name(number: u32) -> String {
    PLATFORMS
        .iter()
        .find(|it| it.1 == number)
        .map_or_else(|| format!("platform {number}"), |it| it.0.to_owned())
}

/// A v4 target, `<arch>-<platform>` (`arm64e.x1-maccatalyst`, `x86_64-ios-simulator`).
fn v4_target(text: &str) -> Result<Target, String> {
    let (arch, platform) =
        text.split_once('-').ok_or_else(|| format!("target `{text}` is not `<arch>-<platform>`"))?;
    let platform = PLATFORMS.iter().find(|it| it.0 == platform).map_or(0, |it| it.1);
    Ok(Target { arch: arch.to_owned(), platform })
}

/// The platforms a v1 to v3 document's `platform` stands for with `arch`: those versions
/// name no simulator, which is the platform on an Intel arch.
fn v3_platforms(name: &str, arch: &str) -> &'static [u32] {
    let intel = matches!(arch, "x86_64" | "x86_64h" | "i386");
    match name {
        "macosx" | "macos" => &[1],
        "ios" if intel => &[7],
        "ios" => &[2],
        "tvos" if intel => &[8],
        "tvos" => &[3],
        "watchos" if intel => &[9],
        "watchos" => &[4],
        "bridgeos" => &[5],
        "iosmac" | "maccatalyst" => &[6],
        "zippered" => &[1, 6],
        "ios-simulator" => &[7],
        "tvos-simulator" => &[8],
        "watchos-simulator" => &[9],
        "driverkit" => &[10],
        "xros" | "visionos" => &[11],
        "xros-simulator" | "visionos-simulator" => &[12],
        _ => &[],
    }
}

// The YAML subset.

#[derive(Clone, PartialEq, Debug)]
enum Value {
    Scalar(String),
    Seq(Vec<Value>),
    Map(Vec<(String, Value)>),
}

fn get<'v>(map: &'v [(String, Value)], key: &str) -> Option<&'v Value> {
    map.iter().find(|it| it.0 == key).map(|it| &it.1)
}

fn scalar<'v>(value: &'v Value, key: &str) -> Result<&'v str, String> {
    match value {
        Value::Scalar(it) => Ok(it),
        _ => Err(format!("`{key}` is not a scalar")),
    }
}

/// A sequence of scalars; a lone scalar is a sequence of one, an empty value of none.
fn scalars<'v>(value: &'v Value, key: &str) -> Result<Vec<&'v str>, String> {
    match value {
        Value::Scalar(it) if it.is_empty() => Ok(Vec::new()),
        Value::Scalar(it) => Ok(vec![it.as_str()]),
        Value::Seq(items) => items.iter().map(|it| scalar(it, key)).collect(),
        Value::Map(_) => Err(format!("`{key}` is not a sequence")),
    }
}

/// The documents of a stream: each one's tag (`!tapi-tbd-v3`) and its lines, numbered from 1.
fn documents(text: &str) -> Result<Vec<(Option<String>, Vec<(usize, &str)>)>, String> {
    let mut docs = Vec::new();
    let mut open: Option<(Option<String>, Vec<(usize, &str)>)> = None;
    for (index, line) in text.lines().enumerate() {
        let number = index + 1;
        if line == "---" || line.starts_with("--- ") || line.starts_with("---\t") {
            if let Some(doc) = open.take() {
                docs.push(doc);
            }
            let tag = strip_comment(&line[3..])?.trim().to_owned();
            open = Some(((!tag.is_empty()).then_some(tag), Vec::new()));
        } else if line.trim_end() == "..." {
            if let Some(doc) = open.take() {
                docs.push(doc);
            }
        } else if let Some((_, lines)) = &mut open {
            lines.push((number, line));
        } else if !line.starts_with('%') && !strip_comment(line)?.trim().is_empty() {
            open = Some((None, vec![(number, line)]));
        }
    }
    if let Some(doc) = open {
        docs.push(doc);
    }
    Ok(docs)
}

/// A logical line: blank lines and comments dropped, a flow collection that wraps joined into
/// the line it starts on.
#[derive(Clone, Debug)]
struct Line {
    number: usize,
    indent: usize,
    text: String,
}

fn logical_lines(lines: &[(usize, &str)]) -> Result<Vec<Line>, String> {
    let mut out: Vec<Line> = Vec::new();
    let mut depth = 0i32;
    for &(number, raw) in lines {
        let text = strip_comment(raw).map_err(|what| format!("line {number}: {what}"))?;
        let trimmed = text.trim();
        if trimmed.is_empty() {
            continue;
        }
        let delta = flow_depth(trimmed);
        if depth > 0 {
            let last = out.last_mut().expect("an open flow collection has a line");
            last.text.push(' ');
            last.text.push_str(trimmed);
        } else {
            let indent = text.len() - text.trim_start_matches(' ').len();
            if text[indent..].starts_with('\t') {
                return Err(format!("line {number}: a tab in the indentation"));
            }
            out.push(Line { number, indent, text: trimmed.to_owned() });
        }
        depth += delta;
        if depth < 0 {
            return Err(format!("line {number}: an unopened `]` or `}}`"));
        }
    }
    if depth > 0 {
        let number = out.last().map_or(0, |it| it.number);
        return Err(format!("line {number}: an unclosed `[` or `{{`"));
    }
    Ok(out)
}

/// Whether a quote at byte `at` of `text` starts a quoted scalar: at the start of a token.
fn opens_quote(text: &str, at: usize) -> bool {
    text[..at].chars().next_back().is_none_or(|it| it.is_whitespace() || "[{,:-".contains(it))
}

/// `text` without a comment: a `#` at the start or after a space, outside quotes.
fn strip_comment(text: &str) -> Result<&str, String> {
    let mut quote = None;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match quote {
            Some('\'') => {
                if c == '\'' {
                    if chars.peek().is_some_and(|it| it.1 == '\'') {
                        chars.next();
                    } else {
                        quote = None;
                    }
                }
            }
            Some(_) => match c {
                '\\' => {
                    chars.next();
                }
                '"' => quote = None,
                _ => {}
            },
            None => match c {
                '\'' | '"' if opens_quote(text, at) => quote = Some(c),
                '#' if text[..at].chars().next_back().is_none_or(char::is_whitespace) => {
                    return Ok(&text[..at]);
                }
                _ => {}
            },
        }
    }
    if quote.is_some() {
        return Err("an unterminated quoted scalar".to_owned());
    }
    Ok(text)
}

/// How many flow collections `text` (a line without its comment) opens less closes.
fn flow_depth(text: &str) -> i32 {
    let mut depth = 0;
    let mut quote = None;
    let mut chars = text.char_indices().peekable();
    while let Some((at, c)) = chars.next() {
        match quote {
            Some('\'') => {
                if c == '\'' {
                    if chars.peek().is_some_and(|it| it.1 == '\'') {
                        chars.next();
                    } else {
                        quote = None;
                    }
                }
            }
            Some(_) => match c {
                '\\' => {
                    chars.next();
                }
                '"' => quote = None,
                _ => {}
            },
            None => match c {
                '\'' | '"' if opens_quote(text, at) => quote = Some(c),
                '[' | '{' => depth += 1,
                ']' | '}' => depth -= 1,
                _ => {}
            },
        }
    }
    depth
}

fn parse_block(lines: &[(usize, &str)]) -> Result<Value, String> {
    let lines = logical_lines(lines)?;
    if lines.is_empty() {
        return Ok(Value::Scalar(String::new()));
    }
    let mut parser = Block { lines, at: 0 };
    let value = parser.block()?;
    if let Some(line) = parser.lines.get(parser.at) {
        return Err(format!("line {}: unexpected indentation", line.number));
    }
    Ok(value)
}

struct Block {
    lines: Vec<Line>,
    at: usize,
}

fn is_item(text: &str) -> bool {
    text == "-" || text.starts_with("- ")
}

impl Block {
    fn block(&mut self) -> Result<Value, String> {
        let line = self.lines[self.at].clone();
        if is_item(&line.text) {
            self.sequence(line.indent)
        } else if split_key(&line.text, line.number)?.is_some() {
            self.mapping(line.indent)
        } else {
            self.at += 1;
            flow(&line.text, line.number)
        }
    }

    fn sequence(&mut self, indent: usize) -> Result<Value, String> {
        let mut items = Vec::new();
        while let Some(line) = self.lines.get(self.at) {
            if line.indent != indent || !is_item(&line.text) {
                break;
            }
            let rest = line.text[1..].trim_start().to_owned();
            if rest.is_empty() {
                self.at += 1;
                items.push(self.nested(indent)?);
            } else {
                let line = &mut self.lines[self.at];
                line.indent += line.text.len() - rest.len();
                line.text = rest;
                items.push(self.block()?);
            }
        }
        Ok(Value::Seq(items))
    }

    fn mapping(&mut self, indent: usize) -> Result<Value, String> {
        let mut entries = Vec::new();
        while let Some(line) = self.lines.get(self.at) {
            if line.indent != indent || is_item(&line.text) {
                break;
            }
            let number = line.number;
            let (key, value) = split_key(&line.text, number)?
                .ok_or_else(|| format!("line {number}: expected `key: value`"))?;
            let value = value.to_owned();
            self.at += 1;
            let value = if value.is_empty() {
                match self.lines.get(self.at) {
                    Some(next) if next.indent == indent && is_item(&next.text) => {
                        self.sequence(indent)?
                    }
                    _ => self.nested(indent)?,
                }
            } else {
                flow(&value, number)?
            };
            if entries.iter().any(|(it, _): &(String, Value)| *it == key) {
                return Err(format!("line {number}: `{key}` given twice"));
            }
            entries.push((key, value));
        }
        Ok(Value::Map(entries))
    }

    /// The block indented under a line at `indent`, or an empty value when there is none.
    fn nested(&mut self, indent: usize) -> Result<Value, String> {
        match self.lines.get(self.at) {
            Some(next) if next.indent > indent => self.block(),
            _ => Ok(Value::Scalar(String::new())),
        }
    }
}

/// A block mapping's `key: value` (the value maybe empty), or `None` for a line that is not one.
fn split_key(text: &str, number: usize) -> Result<Option<(String, &str)>, String> {
    if text.starts_with(['[', '{']) {
        return Ok(None);
    }
    let (key, rest) = if text.starts_with(['\'', '"']) {
        let mut parser = Flow { text, at: 0, number };
        let key = parser.quoted()?;
        (key, text[parser.at..].trim_start())
    } else {
        let bytes = text.as_bytes();
        let Some(colon) = (0..bytes.len())
            .find(|&at| bytes[at] == b':' && bytes.get(at + 1).is_none_or(u8::is_ascii_whitespace))
        else {
            return Ok(None);
        };
        (text[..colon].trim_end().to_owned(), &text[colon..])
    };
    match rest.strip_prefix(':') {
        Some(value) if value.is_empty() || value.starts_with(char::is_whitespace) => {
            Ok(Some((key, value.trim())))
        }
        _ => Ok(None),
    }
}

/// A value written on one logical line: a flow collection, a quoted or a plain scalar.
fn flow(text: &str, number: usize) -> Result<Value, String> {
    let mut parser = Flow { text, at: 0, number };
    let value = parser.value(false)?;
    parser.skip_spaces();
    if parser.at != text.len() {
        return Err(format!("line {number}: unexpected `{}`", &text[parser.at..]));
    }
    Ok(value)
}

struct Flow<'a> {
    text: &'a str,
    at: usize,
    number: usize,
}

impl Flow<'_> {
    fn peek(&self) -> Option<char> {
        self.text[self.at..].chars().next()
    }

    fn skip_spaces(&mut self) {
        while self.peek().is_some_and(char::is_whitespace) {
            self.at += 1;
        }
    }

    fn error(&self, what: &str) -> String {
        format!("line {}: {what} at `{}`", self.number, &self.text[self.at..])
    }

    fn value(&mut self, in_flow: bool) -> Result<Value, String> {
        self.skip_spaces();
        match self.peek() {
            Some('[') => {
                self.at += 1;
                let mut items = Vec::new();
                loop {
                    self.skip_spaces();
                    if self.peek() == Some(']') {
                        self.at += 1;
                        break;
                    }
                    items.push(self.value(true)?);
                    self.skip_spaces();
                    match self.peek() {
                        Some(',') => self.at += 1,
                        Some(']') => {
                            self.at += 1;
                            break;
                        }
                        _ => return Err(self.error("expected `,` or `]`")),
                    }
                }
                Ok(Value::Seq(items))
            }
            Some('{') => {
                self.at += 1;
                let mut entries = Vec::new();
                loop {
                    self.skip_spaces();
                    if self.peek() == Some('}') {
                        self.at += 1;
                        break;
                    }
                    let key = match self.peek() {
                        Some('\'' | '"') => self.quoted()?,
                        _ => self.plain(true, true),
                    };
                    self.skip_spaces();
                    if self.peek() != Some(':') {
                        return Err(self.error("expected `:`"));
                    }
                    self.at += 1;
                    self.skip_spaces();
                    let value = match self.peek() {
                        Some(',' | '}') => Value::Scalar(String::new()),
                        _ => self.value(true)?,
                    };
                    entries.push((key, value));
                    self.skip_spaces();
                    match self.peek() {
                        Some(',') => self.at += 1,
                        Some('}') => {
                            self.at += 1;
                            break;
                        }
                        _ => return Err(self.error("expected `,` or `}`")),
                    }
                }
                Ok(Value::Map(entries))
            }
            Some('\'' | '"') => Ok(Value::Scalar(self.quoted()?)),
            _ => Ok(Value::Scalar(self.plain(in_flow, false))),
        }
    }

    /// A plain scalar: to the end of the line, or in a flow collection to the next `,`, `]`
    /// or `}` (and `:` for a key).
    fn plain(&mut self, in_flow: bool, key: bool) -> String {
        let start = self.at;
        while let Some(c) = self.peek() {
            if (in_flow && matches!(c, ',' | ']' | '}')) || (key && c == ':') {
                break;
            }
            self.at += c.len_utf8();
        }
        self.text[start..self.at].trim().to_owned()
    }

    fn quoted(&mut self) -> Result<String, String> {
        let quote = self.peek().expect("a quote");
        self.at += 1;
        let mut out = String::new();
        loop {
            let Some(c) = self.peek() else {
                return Err(self.error("an unterminated quoted scalar"));
            };
            self.at += c.len_utf8();
            match (quote, c) {
                ('\'', '\'') => {
                    if self.peek() == Some('\'') {
                        self.at += 1;
                        out.push('\'');
                    } else {
                        return Ok(out);
                    }
                }
                ('"', '"') => return Ok(out),
                ('"', '\\') => out.push(self.escape()?),
                _ => out.push(c),
            }
        }
    }

    fn escape(&mut self) -> Result<char, String> {
        let Some(c) = self.peek() else { return Err(self.error("an unterminated escape")) };
        self.at += c.len_utf8();
        let digits = match c {
            '0' => return Ok('\0'),
            't' => return Ok('\t'),
            'n' => return Ok('\n'),
            'r' => return Ok('\r'),
            '"' | '\\' | '/' | ' ' => return Ok(c),
            'x' => 2,
            'u' => 4,
            'U' => 8,
            _ => return Err(self.error("an unknown escape")),
        };
        let hex = self.text.get(self.at..self.at + digits).ok_or_else(|| self.error("a short escape"))?;
        let code = u32::from_str_radix(hex, 16).map_err(|_| self.error("a bad escape"))?;
        self.at += digits;
        char::from_u32(code).ok_or_else(|| self.error("an escape of no character"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sym(name: &str, kind: SymbolKind, weak: bool) -> SharedSymbol {
        SharedSymbol { name: name.to_owned(), version: None, kind, weak }
    }

    fn names(shared: &Shared) -> Vec<&str> {
        shared.symbols.iter().map(|it| it.name.as_str()).collect()
    }

    const V4: &str = "\
--- !tapi-tbd
tbd-version:     4
targets:         [ x86_64-macos, arm64-macos,
                   arm64-ios ]
install-name:    '/usr/lib/libfoo.dylib'
current-version: 1.2
# a comment line
reexported-libraries:
  - targets:         [ x86_64-macos, arm64-macos ]
    libraries:       [ '/usr/lib/system/libbar.dylib',
                       \"/usr/lib/libmissing.dylib\" ]
exports:
  - targets:         [ x86_64-macos, arm64-macos ]   # both
    symbols:         [ _foo, '_quoted$name', 'it''s',
                       _bar ]
    objc-classes:    [ NSFoo ]
    weak-symbols:    [ _weakling ]
    thread-local-symbols: [ _tlv ]
  - targets:         [ arm64-ios ]
    symbols:         [ _ios_only ]
  - targets:         [ x86_64-macos ]
    symbols:         [ _intel_only ]
reexports:
  - targets:         [ arm64-macos ]
    symbols:         [ _reexported_sym ]
...
--- !tapi-tbd
tbd-version:     4
targets:         [ x86_64-macos, arm64e-macos ]
install-name:    '/usr/lib/system/libbar.dylib'
parent-umbrella:
  - targets:         [ x86_64-macos, arm64e-macos ]
    umbrella:        foo
exports:
  - targets:         [ x86_64-macos, arm64e-macos ]
    symbols:         [ _from_bar, _foo ]
    objc-eh-types:   [ BarErr ]
    objc-ivars:      [ Bar._x ]
...
";

    #[test]
    fn v4_arm64_macos() {
        let shared = read("libfoo.tbd", V4, Arch::Aarch64, 1).unwrap();
        assert_eq!(shared.name, "/usr/lib/libfoo.dylib");
        assert_eq!(
            shared.symbols,
            vec![
                sym("foo", SymbolKind::Other, false),
                sym("quoted$name", SymbolKind::Other, false),
                sym("\x01it's", SymbolKind::Other, false),
                sym("bar", SymbolKind::Other, false),
                sym("OBJC_CLASS_$_NSFoo", SymbolKind::Other, false),
                sym("OBJC_METACLASS_$_NSFoo", SymbolKind::Other, false),
                sym("weakling", SymbolKind::Other, true),
                sym("tlv", SymbolKind::Tls, false),
                sym("reexported_sym", SymbolKind::Other, false),
                sym("from_bar", SymbolKind::Other, false),
                sym("OBJC_EHTYPE_$_BarErr", SymbolKind::Other, false),
                sym("OBJC_IVAR_$_Bar._x", SymbolKind::Other, false),
            ]
        );
        assert_eq!(shared.inputs, vec!["/usr/lib/libmissing.dylib".to_owned()]);
        assert!(shared.more.is_empty());
    }

    #[test]
    fn v4_x86_64_macos() {
        let shared = read("libfoo.tbd", V4, Arch::X86_64, 1).unwrap();
        let names = names(&shared);
        assert!(names.contains(&"intel_only"));
        assert!(names.contains(&"from_bar"));
        assert!(!names.contains(&"reexported_sym"));
        assert!(!names.contains(&"ios_only"));
    }

    #[test]
    fn v4_filters_platform() {
        let shared = read("libfoo.tbd", V4, Arch::Aarch64, 2).unwrap();
        assert_eq!(names(&shared), vec!["ios_only"]);
        assert!(shared.inputs.is_empty());
    }

    #[test]
    fn v4_missing_target() {
        let error = read("libfoo.tbd", V4, Arch::Aarch64, 3).unwrap_err();
        assert!(error.starts_with("libfoo.tbd: "), "{error}");
        assert!(error.contains("arm64-tvos"), "{error}");
        assert!(error.contains("x86_64-macos, arm64-macos, arm64-ios"), "{error}");
    }

    #[test]
    fn v4_exact_arch_wins() {
        let text = "\
--- !tapi-tbd
tbd-version: 4
targets: [ arm64-macos, arm64e-macos ]
install-name: /usr/lib/libexact.dylib
exports:
  - targets: [ arm64-macos ]
    symbols: [ _plain ]
  - targets: [ arm64e-macos ]
    symbols: [ _authenticated ]
...
";
        let shared = read("exact.tbd", text, Arch::Aarch64, 1).unwrap();
        assert_eq!(names(&shared), vec!["plain"]);
    }

    const V3: &str = "\
--- !tapi-tbd-v3
archs:           [ x86_64, arm64 ]
uuids:           [ 'x86_64: 00000000-0000-0000-0000-000000000000',
                   'arm64: 11111111-1111-1111-1111-111111111111' ]
platform:        macosx
install-name:    /usr/lib/libv3.dylib
current-version: 1
exports:
  - archs:           [ x86_64, arm64 ]
    re-exports:      [ /usr/lib/libv3sub.dylib ]
    symbols:         [ _common, \"_double\\x41\" ]
    objc-classes:    [ V3Class ]
    objc-ivars:      [ V3Class.ivar ]
    weak-def-symbols: [ _weak3 ]
    thread-local-symbols: [ _tls3 ]
  - archs:           [ x86_64 ]
    symbols:         [ _x86_only ]
...
--- !tapi-tbd-v3
archs:           [ x86_64, arm64 ]
platform:        macosx
install-name:    /usr/lib/libv3sub.dylib
exports:
  - archs:           [ arm64 ]
    symbols:         [ _sub_arm ]
...
";

    #[test]
    fn v3_arm64_macos() {
        let shared = read("libv3.tbd", V3, Arch::Aarch64, 1).unwrap();
        assert_eq!(shared.name, "/usr/lib/libv3.dylib");
        assert_eq!(
            shared.symbols,
            vec![
                sym("common", SymbolKind::Other, false),
                sym("doubleA", SymbolKind::Other, false),
                sym("OBJC_CLASS_$_V3Class", SymbolKind::Other, false),
                sym("OBJC_METACLASS_$_V3Class", SymbolKind::Other, false),
                sym("OBJC_IVAR_$_V3Class.ivar", SymbolKind::Other, false),
                sym("weak3", SymbolKind::Other, true),
                sym("tls3", SymbolKind::Tls, false),
                sym("sub_arm", SymbolKind::Other, false),
            ]
        );
        assert!(shared.inputs.is_empty());
    }

    #[test]
    fn v3_x86_64_and_missing() {
        let shared = read("libv3.tbd", V3, Arch::X86_64, 1).unwrap();
        let names = names(&shared);
        assert!(names.contains(&"x86_only"));
        assert!(!names.contains(&"sub_arm"));
        let error = read("libv3.tbd", V3, Arch::Aarch64, 2).unwrap_err();
        assert!(error.contains("arm64-ios") && error.contains("arm64 (macosx)"), "{error}");
    }

    #[test]
    fn v2_objc_names_drop_underscore() {
        let text = "\
--- !tapi-tbd-v2
archs: [ x86_64 ]
platform: macosx
install-name: /usr/lib/libv2.dylib
exports:
  - archs: [ x86_64 ]
    objc-classes: [ _Old ]
...
";
        let shared = read("libv2.tbd", text, Arch::X86_64, 1).unwrap();
        assert_eq!(names(&shared), vec!["OBJC_CLASS_$_Old", "OBJC_METACLASS_$_Old"]);
    }

    #[test]
    fn errors() {
        let unknown = "--- !tapi-tbd\ntbd-version: 4\ntargets: [ arm64-macos ]\ninstall-name: /x\n\
                       exports:\n  - targets: [ arm64-macos ]\n    strange: [ _a ]\n...\n";
        let error = read("u.tbd", unknown, Arch::Aarch64, 1).unwrap_err();
        assert!(error.contains("unknown key `strange`"), "{error}");
        let v5 = "--- !tapi-tbd\ntbd-version: 5\ntargets: [ arm64-macos ]\ninstall-name: /x\n...\n";
        assert!(read("v5.tbd", v5, Arch::Aarch64, 1).unwrap_err().contains("tbd-version 5"));
        assert!(read("j.tbd", "{ \"tapi_tbd_version\": 5 }", Arch::Aarch64, 1).is_err());
        let open = "--- !tapi-tbd\ntbd-version: 4\ntargets: [ arm64-macos\n";
        assert!(read("o.tbd", open, Arch::Aarch64, 1).unwrap_err().contains("unclosed"));
        assert!(read("e.tbd", "", Arch::Aarch64, 1).is_err());
    }

    #[test]
    fn yaml_subset() {
        let text = "\
a: plain value # comment
'b c': [ x, 'y, z', \"q\\\"\" ,
         w ]
d:
- one
- k: v
  l: { m: n, o: [ p ] }
e:
";
        let lines: Vec<(usize, &str)> = text.lines().enumerate().map(|(i, l)| (i + 1, l)).collect();
        let s = |it: &str| Value::Scalar(it.to_owned());
        assert_eq!(
            parse_block(&lines).unwrap(),
            Value::Map(vec![
                ("a".to_owned(), s("plain value")),
                ("b c".to_owned(), Value::Seq(vec![s("x"), s("y, z"), s("q\""), s("w")])),
                (
                    "d".to_owned(),
                    Value::Seq(vec![
                        s("one"),
                        Value::Map(vec![
                            ("k".to_owned(), s("v")),
                            (
                                "l".to_owned(),
                                Value::Map(vec![
                                    ("m".to_owned(), s("n")),
                                    ("o".to_owned(), Value::Seq(vec![s("p")])),
                                ]),
                            ),
                        ]),
                    ]),
                ),
                ("e".to_owned(), s("")),
            ])
        );
    }

    fn sdk_libsystem() -> Option<(String, String)> {
        let mut roots = Vec::new();
        if let Ok(root) = std::env::var("SDKROOT") {
            roots.push(root);
        }
        roots.push("/Library/Developer/CommandLineTools/SDKs/MacOSX.sdk".to_owned());
        roots.push(
            "/Applications/Xcode.app/Contents/Developer/Platforms/MacOSX.platform/Developer/SDKs/MacOSX.sdk"
                .to_owned(),
        );
        roots.into_iter().find_map(|root| {
            let path = format!("{root}/usr/lib/libSystem.B.tbd");
            std::fs::read_to_string(&path).ok().map(|text| (path, text))
        })
    }

    #[test]
    fn sdk_libsystem_tbd() {
        let Some((path, text)) = sdk_libsystem() else {
            eprintln!("no macOS SDK: skipping");
            return;
        };
        let binder = super::super::core_name(b"dyld_stub_binder").unwrap();
        for arch in [Arch::Aarch64, Arch::X86_64] {
            let shared = read(&path, &text, arch, 1).unwrap();
            assert_eq!(shared.name, "/usr/lib/libSystem.B.dylib");
            let names: FxHashSet<&str> = shared.symbols.iter().map(|it| it.name.as_str()).collect();
            for name in ["malloc", "printf", "pthread_create", "_tlv_bootstrap", binder.as_str()] {
                assert!(names.contains(name), "{arch:?}: no `{name:?}` in {path}");
            }
        }
    }
}
