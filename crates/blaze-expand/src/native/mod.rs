//! Native expanders: proc macros blaze runs as code of its own rather than as a module it
//! builds of the macro's crate, for a crate that is, with every package it depends on, a copy
//! blaze verified, byte for byte ([`find`]). Then the expander is the macro's own code: the
//! package's published sources, compiled into blaze as libraries ([`verified`]) against a
//! `proc_macro` that is blaze's WebAssembly guest's with its imports answered in process
//! (`proc_macro/`, [`host`]). A macro sees what it sees as a module: the same spans, numbered
//! the same, the same answers to what it asks of them, the same text of a stream, so its
//! output is the module's token for token and span for span. `BLAZE_NATIVE_MACROS=verify`
//! expands every call both ways and fails on the first [`difference`].
//!
//! The rule is an exact match, not a fallback: a library is the verified copy when its crate
//! and every package it depends on (but the standard library's) has the name, the version
//! and every file, by the BLAKE3 of its bytes, of the copy, is built with the same features,
//! and depends on the same packages under the same names. Any other is expanded as it would
//! be with no native expander at all, a miss [`report`] counts by its first difference. Of
//! what else a package is compiled with, a cfg that no source of it or of what it depends on
//! names (as `debug_assertions` in a dev profile) is not compared: it can't change the code.
//!
//! The packages are serde_derive's (`Serialize`, `Deserialize`), thiserror-impl's 1.x and
//! 2.x (`Error`) and tokio-macros' (`main`, `test` and the rest), over syn 2 and 3, quote,
//! proc-macro2 and unicode-ident (`docs/v2/usercode-macros-native-notes.md`). bitflags'
//! `bitflags!` has none: it is a `macro_rules!` macro, which the front end already expands
//! natively, and taking over more of it than its first step would drop the expansions whose
//! marks its output's syntax contexts carry.

mod files;
mod host;
#[allow(dead_code)]
mod lex;
mod verified;
#[allow(dead_code)]
mod wire;

#[cfg(test)]
mod tests;

use std::{
    cmp::Ordering,
    collections::VecDeque,
    path::{Path, PathBuf},
    sync::{Mutex, MutexGuard, OnceLock},
    time::SystemTime,
};

use base_db::{Env, SourceDatabase};
use blaze_ids::{ContentId, Hasher};
use hir_expand::proc_macro::{ProcMacroExpansionError, ProcMacroKind};
use paths::AbsPath;
use rustc_hash::{FxHashMap, FxHashSet};
use span::Span;
use tt::{Leaf, TokenTree, TopSubtree};

use self::{
    host::Host,
    verified::{Build, Expander, PACKAGES, ROOTS},
    wire::{Reader, Spans, Writer},
};

/// A proc-macro crate and every package it depends on but the standard library's, as
/// [`find`] holds it to the copy blaze verified.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Closure {
    /// The macro's crate first, then every package it depends on, directly or not, once.
    pub packages: Vec<Package>,
}

/// A package of a [`Closure`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Package {
    /// As its `Cargo.toml` names it (`CARGO_PKG_NAME`).
    pub name: String,
    pub version: String,
    /// Every file of its directory but what cargo writes beside a package it unpacks
    /// (`.cargo-ok`, `.cargo-checksum.json`), by its path from the directory with `/` between
    /// components, and the BLAKE3 of its bytes: sorted by path.
    pub files: Vec<(String, [u8; 32])>,
    /// What it is compiled with, as a `rust-project.json` has it: `feature="name"` for each
    /// feature and what its build script set.
    pub cfg: Vec<String>,
    /// Each dependency: the name its code knows it by, and its index in
    /// [`Closure::packages`].
    pub deps: Vec<(String, usize)>,
}

/// What [`find`] is told of the library of proc macros at a path: its closure, `None` when no
/// native expander can be of it (it was not built from a description, or its crate is none
/// blaze verified, [`closure_of_description`]), and `Err` when it can't be told.
pub type Describe = dyn Fn(&AbsPath) -> Result<Option<Closure>, String> + Send + Sync;

static DESCRIBE: OnceLock<Box<Describe>> = OnceLock::new();

/// Has [`find`] learn each library's closure from `describe`, for every load from here on.
/// Says so when one was registered before.
pub fn describe_with(describe: Box<Describe>) -> Result<(), String> {
    DESCRIBE
        .set(describe)
        .map_err(|_| "the description of proc-macro libraries was registered twice".to_owned())
}

/// The native expander of the macro `name` of `kind` of the library at `dylib`: `None` when
/// the library is not of a verified copy. `Err` when the library can't be described, or is the
/// verified copy of a crate that declares no such macro.
pub fn find(dylib: &AbsPath, name: &str, kind: ProcMacroKind) -> Result<Option<Macro>, String> {
    let describe = DESCRIBE.get().ok_or(
        "nothing describes a library of proc macros to blaze's native expanders \
         (blaze_expand::native::describe_with)",
    )?;
    let Some(closure) = describe(dylib)? else { return Ok(None) };
    shaped(&closure).map_err(|why| format!("the description of {dylib}: {why}"))?;
    let crate_ = &closure.packages[0];
    let Some(root) = ROOTS.iter().position(|it| {
        let package = &PACKAGES[it.package];
        package.name == crate_.name && package.version == crate_.version
    }) else {
        return Ok(None);
    };
    if let Err(why) = matches(&closure, ROOTS[root].package) {
        missed(format!("{} {}: {why}", crate_.name, crate_.version));
        return Ok(None);
    }
    let entry =
        ROOTS[root].macros.iter().position(|it| it.name == name && it.kind == kind).ok_or_else(
            || {
                format!(
                    "{dylib} is the verified copy of {} {}, which declares no {kind:?} macro \
                     `{name}`",
                    crate_.name, crate_.version
                )
            },
        )?;
    lock(&REPORT).found += 1;
    Ok(Some(Macro { root, entry, identity: identity(root, entry) }))
}

/// A macro blaze has a native expander of.
#[derive(Clone, Debug)]
pub struct Macro {
    /// Its crate, of [`ROOTS`], and itself, of the crate's macros.
    root: usize,
    entry: usize,
    identity: ContentId,
}

impl Macro {
    /// What its expansions are a function of: the verified copy of its crate and every
    /// package it depends on, what they are built with, the code that runs them, its name and
    /// its kind.
    pub fn identity(&self) -> ContentId {
        self.identity
    }

    /// Expands a call as the macro's module would. The macro reads none of the crate's
    /// variables (`env`): no package of [`verified`] reads the environment as it runs.
    #[allow(clippy::too_many_arguments)]
    pub fn expand(
        &self,
        db: &dyn SourceDatabase,
        subtree: &TopSubtree,
        attrs: Option<&TopSubtree>,
        _env: &Env,
        def_site: Span,
        call_site: Span,
        mixed_site: Span,
    ) -> Result<TopSubtree, ProcMacroExpansionError> {
        self.expand_with(Some(db), subtree, attrs, def_site, call_site, mixed_site)
    }

    fn expand_with(
        &self,
        db: Option<&dyn SourceDatabase>,
        subtree: &TopSubtree,
        attrs: Option<&TopSubtree>,
        def_site: Span,
        call_site: Span,
        mixed_site: Span,
    ) -> Result<TopSubtree, ProcMacroExpansionError> {
        let entry = &ROOTS[self.root].macros[self.entry];
        // As the wasm host numbers them: the sites 1, 2 and 3, the same spans or not, then
        // the input's as they first appear.
        let mut spans = Spans::default();
        for span in [def_site, call_site, mixed_site] {
            spans.push(span);
        }
        let mut writer = Writer { out: Vec::new(), spans: &mut spans };
        writer.top(subtree);
        let input = std::mem::take(&mut writer.out);
        let attr = attrs.map(|it| {
            writer.top(it);
            std::mem::take(&mut writer.out)
        });
        let mut host = Host { db, spans, sites: [1, 2, 3] };
        let output = native_proc_macro::__native::with_host(&mut host, || match entry.expander {
            Expander::One(f) => native_proc_macro::__native::expand1(f, &input),
            // rust-analyzer gives an attribute written without arguments (`#[tokio::main]`)
            // none, which the macro is given as an empty stream.
            Expander::Two(f) => {
                let attr = attr.unwrap_or_else(|| 0u32.to_le_bytes().to_vec());
                native_proc_macro::__native::expand2(f, &attr, &input)
            }
        });
        let output = output.map_err(ProcMacroExpansionError::Panic)?;
        let trees = Reader::new(&output, &host.spans)
            .stream()
            .map_err(|e| ProcMacroExpansionError::System(format!("`{}`: {e}", entry.name)))?;
        Ok(wire::top_subtree(trees, call_site))
    }
}

/// The first difference of a native expander's output from the macro's, or `None` when they
/// are the same: every token tree, each delimiter, character, spacing, text, literal kind and
/// suffix, and of every span its range, anchor and syntax context.
pub fn difference(native: &TopSubtree, normal: &TopSubtree) -> Option<String> {
    let natively: Vec<TokenTree> = native.as_token_trees().iter_flat_tokens().collect();
    let normally: Vec<TokenTree> = normal.as_token_trees().iter_flat_tokens().collect();
    for (at, (ours, theirs)) in natively.iter().zip(&normally).enumerate() {
        if let Some(what) = tree_difference(ours, theirs) {
            return Some(format!("token tree {at}, {}: {what}", shown(theirs)));
        }
    }
    match natively.len().cmp(&normally.len()) {
        Ordering::Equal => None,
        Ordering::Less => Some(format!(
            "the native output ends after {} token trees, where the macro's goes on with {}",
            natively.len(),
            shown(&normally[natively.len()]),
        )),
        Ordering::Greater => Some(format!(
            "the native output goes on after the macro's {} token trees with {}",
            normally.len(),
            shown(&natively[normally.len()]),
        )),
    }
}

fn tree_difference(ours: &TokenTree, theirs: &TokenTree) -> Option<String> {
    match (ours, theirs) {
        (TokenTree::Subtree(ours), TokenTree::Subtree(theirs)) => {
            let (a, b) = (ours.delimiter, theirs.delimiter);
            if a.kind != b.kind {
                return Some(format!(
                    "its delimiter is {:?} natively and {:?} by the macro",
                    a.kind, b.kind
                ));
            }
            span_difference("its opening delimiter's", a.open, b.open)
                .or_else(|| span_difference("its closing delimiter's", a.close, b.close))
                .or_else(|| {
                    (ours.len != theirs.len).then(|| {
                        format!(
                            "it holds {} token trees natively and {} by the macro",
                            ours.len, theirs.len
                        )
                    })
                })
        }
        (TokenTree::Leaf(ours), TokenTree::Leaf(theirs)) => leaf_difference(ours, theirs),
        _ => Some(format!(
            "it is {} natively and {} by the macro",
            tree_kind(ours),
            tree_kind(theirs)
        )),
    }
}

fn leaf_difference(ours: &Leaf, theirs: &Leaf) -> Option<String> {
    let differs = |what: &str, a: &dyn std::fmt::Debug, b: &dyn std::fmt::Debug| {
        Some(format!("its {what} is {a:?} natively and {b:?} by the macro"))
    };
    match (ours, theirs) {
        (Leaf::Ident(a), Leaf::Ident(b)) => {
            if a.sym != b.sym {
                return differs("text", &a.sym.as_str(), &b.sym.as_str());
            }
            if a.is_raw != b.is_raw {
                return differs("rawness", &a.is_raw, &b.is_raw);
            }
            span_difference("its", a.span, b.span)
        }
        (Leaf::Punct(a), Leaf::Punct(b)) => {
            if a.char != b.char {
                return differs("character", &a.char, &b.char);
            }
            if a.spacing != b.spacing {
                return differs("spacing", &a.spacing, &b.spacing);
            }
            span_difference("its", a.span, b.span)
        }
        (Leaf::Literal(a), Leaf::Literal(b)) => {
            if a.kind != b.kind {
                return differs("literal kind", &a.kind, &b.kind);
            }
            if a.text() != b.text() {
                return differs("text", &a.text(), &b.text());
            }
            if a.suffix() != b.suffix() {
                return differs("suffix", &a.suffix(), &b.suffix());
            }
            span_difference("its", a.span, b.span)
        }
        _ => Some(format!(
            "it is {} natively and {} by the macro",
            leaf_kind(ours),
            leaf_kind(theirs)
        )),
    }
}

fn span_difference(whose: &str, ours: Span, theirs: Span) -> Option<String> {
    if ours.range != theirs.range {
        return Some(format!(
            "{whose} range is {:?} natively and {:?} by the macro",
            ours.range, theirs.range
        ));
    }
    if ours.anchor != theirs.anchor {
        return Some(format!(
            "{whose} anchor is {:?} natively and {:?} by the macro",
            ours.anchor, theirs.anchor
        ));
    }
    if ours.ctx != theirs.ctx {
        return Some(format!(
            "{whose} syntax context is {:?} natively and {:?} by the macro",
            ours.ctx, theirs.ctx
        ));
    }
    None
}

fn tree_kind(tree: &TokenTree) -> &'static str {
    match tree {
        TokenTree::Subtree(_) => "a group",
        TokenTree::Leaf(leaf) => leaf_kind(leaf),
    }
}

fn leaf_kind(leaf: &Leaf) -> &'static str {
    match leaf {
        Leaf::Ident(_) => "an identifier",
        Leaf::Punct(_) => "a punctuation mark",
        Leaf::Literal(_) => "a literal",
    }
}

/// A token tree as a reader finds it in an error.
fn shown(tree: &TokenTree) -> String {
    match tree {
        TokenTree::Subtree(subtree) => match subtree.delimiter.kind {
            tt::DelimiterKind::Parenthesis => "the group `(`".to_owned(),
            tt::DelimiterKind::Brace => "the group `{`".to_owned(),
            tt::DelimiterKind::Bracket => "the group `[`".to_owned(),
            tt::DelimiterKind::Invisible => "a group without delimiters".to_owned(),
        },
        TokenTree::Leaf(Leaf::Ident(it)) => format!("`{}{}`", it.is_raw.as_str(), it.sym.as_str()),
        TokenTree::Leaf(Leaf::Punct(it)) => format!("`{}`", it.char),
        TokenTree::Leaf(Leaf::Literal(it)) => format!("the literal `{}{}`", it.text(), it.suffix()),
    }
}

/// The closure of the proc-macro crate a `rust-project.json` describes first, as blaze writes
/// one for the crate's module (`load-cargo/src/described.rs`): each crate's package is the
/// directory its `CARGO_MANIFEST_DIR` names. `None`, reading no file, when the crate is none
/// blaze verified a copy of.
pub fn closure_of_description(description: &str) -> Result<Option<Closure>, String> {
    let value: serde_json::Value = serde_json::from_str(description)
        .map_err(|e| format!("the description is not JSON: {e}"))?;
    let crates = value["crates"].as_array().ok_or("the description has no crates")?;
    let described: Vec<Described> = crates
        .iter()
        .enumerate()
        .map(|(at, it)| Described::of(it).map_err(|why| format!("its crate {at}: {why}")))
        .collect::<Result<_, String>>()?;
    let crate_ = described.first().ok_or("the description is of no crate")?;
    let verified = ROOTS.iter().any(|it| {
        let package = &PACKAGES[it.package];
        package.name == crate_.name && package.version == crate_.version
    });
    if !verified {
        return Ok(None);
    }
    let packages = described
        .into_iter()
        .map(|it| {
            let files = package_files(&it.dir)?;
            Ok(Package { name: it.name, version: it.version, files, cfg: it.cfg, deps: it.deps })
        })
        .collect::<Result<_, String>>()?;
    Ok(Some(Closure { packages }))
}

/// A crate of a description, before its package's files are read.
struct Described {
    name: String,
    version: String,
    dir: PathBuf,
    cfg: Vec<String>,
    deps: Vec<(String, usize)>,
}

impl Described {
    fn of(value: &serde_json::Value) -> Result<Described, String> {
        let env = |key: &str| {
            value["env"][key].as_str().map(str::to_owned).ok_or_else(|| format!("no {key}"))
        };
        let dir = PathBuf::from(env("CARGO_MANIFEST_DIR")?);
        if !dir.is_absolute() {
            return Err(format!("its CARGO_MANIFEST_DIR {} is not absolute", dir.display()));
        }
        let mut cfg = Vec::new();
        for atom in value["cfg"].as_array().ok_or("no cfg")? {
            cfg.push(atom.as_str().ok_or("a cfg that is not a string")?.to_owned());
        }
        cfg.sort();
        let mut deps = Vec::new();
        for dep in value["deps"].as_array().ok_or("no deps")? {
            let at = dep["crate"].as_u64().ok_or("a dependency that names no crate")?;
            let name = dep["name"].as_str().ok_or("a dependency without a name")?;
            deps.push((name.to_owned(), at as usize));
        }
        deps.sort();
        Ok(Described {
            name: env("CARGO_PKG_NAME")?,
            version: env("CARGO_PKG_VERSION")?,
            dir,
            cfg,
            deps,
        })
    }
}

/// Every file of the package at `dir`, as [`Package::files`] has them.
fn package_files(dir: &Path) -> Result<Vec<(String, [u8; 32])>, String> {
    let mut files = Vec::new();
    walk(dir, "", &mut files)?;
    files.sort();
    Ok(files)
}

fn walk(dir: &Path, prefix: &str, files: &mut Vec<(String, [u8; 32])>) -> Result<(), String> {
    let read = |e: std::io::Error| format!("reading {}: {e}", dir.display());
    for entry in std::fs::read_dir(dir).map_err(read)? {
        let entry = entry.map_err(read)?;
        let name = entry.file_name();
        // A name that is not UTF-8 is in no published package, so it is kept as one that
        // matches none.
        let name = match name.to_str() {
            Some(name) => name.to_owned(),
            None => format!("\0{}", name.to_string_lossy()),
        };
        if prefix.is_empty() && matches!(name.as_str(), ".cargo-ok" | ".cargo-checksum.json") {
            continue;
        }
        let rel = if prefix.is_empty() { name } else { format!("{prefix}/{name}") };
        let path = entry.path();
        let meta = std::fs::symlink_metadata(&path)
            .map_err(|e| format!("reading {}: {e}", path.display()))?;
        if meta.is_dir() {
            walk(&path, &rel, files)?;
        } else if meta.is_file() {
            files.push((rel, file_hash(&path, &meta)?));
        } else {
            // A link or another kind of file, which no published package has.
            let target = std::fs::read_link(&path).unwrap_or_default();
            let said = format!("\0not a file: {}", target.display());
            files.push((rel, *blake3::hash(said.as_bytes()).as_bytes()));
        }
    }
    Ok(())
}

/// What a file's hash is kept under: what `stat` says of it, which changes when it does.
type Stamp = (u64, Option<SystemTime>, u64, i64, i64);

fn stamp(meta: &std::fs::Metadata) -> Stamp {
    #[cfg(unix)]
    let (inode, changed, changed_nanos) = {
        use std::os::unix::fs::MetadataExt as _;
        (meta.ino(), meta.ctime(), meta.ctime_nsec())
    };
    #[cfg(not(unix))]
    let (inode, changed, changed_nanos) = (0, 0, 0);
    (meta.len(), meta.modified().ok(), inode, changed, changed_nanos)
}

/// The BLAKE3 of the file at `path`, read once for each change of its [`Stamp`].
fn file_hash(path: &Path, meta: &std::fs::Metadata) -> Result<[u8; 32], String> {
    static HASHES: Mutex<Option<FxHashMap<PathBuf, (Stamp, [u8; 32])>>> = Mutex::new(None);
    let stamp = stamp(meta);
    if let Some((seen, hash)) = lock(&HASHES).get_or_insert_default().get(path)
        && *seen == stamp
    {
        return Ok(*hash);
    }
    let bytes = std::fs::read(path).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let hash = *blake3::hash(&bytes).as_bytes();
    lock(&HASHES).get_or_insert_default().insert(path.to_owned(), (stamp, hash));
    Ok(hash)
}

/// Why `closure` is not one: a package it has not, or files out of order.
fn shaped(closure: &Closure) -> Result<(), String> {
    if closure.packages.is_empty() {
        return Err("it is of no package".to_owned());
    }
    for package in &closure.packages {
        if let Some((name, at)) = package.deps.iter().find(|it| it.1 >= closure.packages.len()) {
            return Err(format!(
                "{} {} depends on `{name}`, its package {at}, which it has not",
                package.name, package.version
            ));
        }
        if !package.files.windows(2).all(|it| it[0].0 < it[1].0) {
            return Err(format!(
                "the files of {} {} are not sorted by path, each once",
                package.name, package.version
            ));
        }
    }
    Ok(())
}

/// What blaze runs of a package of [`PACKAGES`]: the BLAKE3 of each file, in its order, and
/// what it is compiled with, or why that can't be known.
struct Actual {
    files: Vec<(&'static str, [u8; 32])>,
    cfg: Result<Vec<String>, String>,
}

fn actual() -> &'static [Actual] {
    static ACTUAL: OnceLock<Vec<Actual>> = OnceLock::new();
    ACTUAL.get_or_init(|| {
        PACKAGES
            .iter()
            .map(|package| Actual {
                files: package
                    .files
                    .iter()
                    .map(|(path, bytes)| (*path, *blake3::hash(bytes).as_bytes()))
                    .collect(),
                cfg: cfg_of(package),
            })
            .collect()
    })
}

fn cfg_of(package: &verified::Package) -> Result<Vec<String>, String> {
    let mut cfg: Vec<String> =
        package.features.iter().map(|it| format!("feature=\"{it}\"")).collect();
    if cfg!(debug_assertions) {
        cfg.push("debug_assertions".to_owned());
    }
    if package.build == Build::ProcMacro2 {
        cfg.extend(proc_macro2_cfg()?.iter().map(|it| (*it).to_owned()));
    }
    cfg.sort();
    Ok(cfg)
}

/// What proc-macro2's build script set for the copy blaze runs, as its code shows it: a span
/// of the call site from `proc_macro` (`wrap_proc_macro`) and a join asked of the host
/// (`proc_macro_span`), which on a compiler that has the one has `proc_macro_span_location`
/// and `proc_macro_span_file` too (its `build.rs`). Without `proc_macro_span` those two are
/// probed for, and nothing the code does shows the answer: then the copy is not known.
fn proc_macro2_cfg() -> Result<&'static [&'static str], String> {
    static CFG: OnceLock<Result<&'static [&'static str], String>> = OnceLock::new();
    CFG.get_or_init(|| {
        let mut probe = Probe::default();
        native_proc_macro::__native::with_host(&mut probe, || {
            let span = native_proc_macro2::Span::call_site();
            _ = span.join(span);
        });
        match (probe.call_site, probe.join) {
            (true, true) => Ok(&[
                "proc_macro_span",
                "proc_macro_span_file",
                "proc_macro_span_location",
                "wrap_proc_macro",
            ][..]),
            (wrapped, _) => Err(format!(
                "blaze's copy of proc-macro2 was built without {}`proc_macro_span`, so whether \
                 it has `proc_macro_span_location` and `proc_macro_span_file` can't be seen",
                if wrapped { "" } else { "`wrap_proc_macro` or " },
            )),
        }
    })
    .clone()
}

/// A host that notes what proc-macro2 asks of it ([`proc_macro2_cfg`]).
#[derive(Default)]
struct Probe {
    call_site: bool,
    join: bool,
}

impl native_proc_macro::__native::Host for Probe {
    fn def_site(&mut self) -> u32 {
        1
    }

    fn call_site(&mut self) -> u32 {
        self.call_site = true;
        2
    }

    fn mixed_site(&mut self) -> u32 {
        3
    }

    fn span_join(&mut self, _a: u32, _b: u32) -> u32 {
        self.join = true;
        0
    }

    fn span_resolved_at(&mut self, a: u32, _at: u32) -> u32 {
        a
    }

    fn span_located_at(&mut self, _a: u32, b: u32) -> u32 {
        b
    }

    fn span_parent(&mut self, _a: u32) -> u32 {
        0
    }

    fn span_source(&mut self, a: u32) -> u32 {
        a
    }

    fn span_start(&mut self, a: u32) -> u32 {
        a
    }

    fn span_end(&mut self, a: u32) -> u32 {
        a
    }

    fn span_subspan(&mut self, _a: u32, _start: u64, _end: u64) -> u32 {
        0
    }

    fn span_byte_range(&mut self, _a: u32) -> [u32; 2] {
        [0, 0]
    }

    fn span_line(&mut self, _a: u32) -> u32 {
        1
    }

    fn span_column(&mut self, _a: u32) -> u32 {
        1
    }

    fn span_debug(&mut self, _a: u32) -> Option<String> {
        None
    }

    fn span_file(&mut self, _a: u32) -> Option<String> {
        None
    }

    fn span_local_file(&mut self, _a: u32) -> Option<String> {
        None
    }

    fn span_source_text(&mut self, _a: u32) -> Option<String> {
        None
    }

    fn ts_from_str(&mut self, _text: &str) -> Result<Vec<u8>, String> {
        Err("a probe lexes nothing".to_owned())
    }

    fn literal_from_str(&mut self, _text: &str) -> Option<Vec<u8>> {
        None
    }
}

/// Why `closure` is not the verified copy whose crate is `root` of [`PACKAGES`]: its first
/// package that differs, from the crate down its dependencies, and how.
fn matches(closure: &Closure, root: usize) -> Result<(), String> {
    let actual = actual();
    let mut paired: FxHashMap<usize, usize> = FxHashMap::default();
    paired.insert(0, root);
    let mut next = VecDeque::from([(0, root)]);
    while let Some((at, ours)) = next.pop_front() {
        let theirs = &closure.packages[at];
        let package = &PACKAGES[ours];
        let named = format!("{} {}", package.name, package.version);
        if theirs.name != package.name || theirs.version != package.version {
            return Err(format!(
                "{} {} is where the verified copy has {named}",
                theirs.name, theirs.version
            ));
        }
        if let Some(why) = files_difference(&theirs.files, &actual[ours].files) {
            return Err(format!("{named}: {why}"));
        }
        let cfg = actual[ours].cfg.as_ref().map_err(|why| format!("{named}: {why}"))?;
        if let Some(why) = cfg_difference(&theirs.cfg, cfg, ours) {
            return Err(format!("{named}: {why}"));
        }
        let mut deps: Vec<&(String, usize)> = theirs.deps.iter().collect();
        deps.sort();
        let names: Vec<&str> = deps.iter().map(|it| it.0.as_str()).collect();
        let verified: Vec<&str> = package.deps.iter().map(|it| it.0).collect();
        if names != verified {
            return Err(format!(
                "{named} depends on [{}] where the verified copy depends on [{}]",
                names.join(", "),
                verified.join(", "),
            ));
        }
        for (dep, verified_dep) in deps.iter().map(|it| it.1).zip(package.deps.iter().map(|it| it.1))
        {
            match paired.get(&dep) {
                Some(&it) if it != verified_dep => {
                    return Err(format!(
                        "one package stands for both {} {} and {} {} of the verified copy",
                        PACKAGES[it].name,
                        PACKAGES[it].version,
                        PACKAGES[verified_dep].name,
                        PACKAGES[verified_dep].version,
                    ));
                }
                Some(_) => {}
                None => {
                    paired.insert(dep, verified_dep);
                    next.push_back((dep, verified_dep));
                }
            }
        }
    }
    Ok(())
}

/// The first file of two sorted lists that one has and the other has not, or whose bytes
/// differ.
fn files_difference(theirs: &[(String, [u8; 32])], ours: &[(&str, [u8; 32])]) -> Option<String> {
    let (mut theirs, mut ours) = (theirs.iter().peekable(), ours.iter().peekable());
    loop {
        match (theirs.peek(), ours.peek()) {
            (None, None) => return None,
            (Some((path, _)), None) => {
                return Some(format!("it has `{path}`, which the verified copy has not"));
            }
            (None, Some((path, _))) => return Some(format!("it has no `{path}`")),
            (Some((path, hash)), Some((verified, verified_hash))) => {
                match path.as_str().cmp(verified) {
                    Ordering::Less => {
                        return Some(format!("it has `{path}`, which the verified copy has not"));
                    }
                    Ordering::Greater => return Some(format!("it has no `{verified}`")),
                    Ordering::Equal if hash != verified_hash => {
                        return Some(format!("its `{path}` is not the verified copy's"));
                    }
                    Ordering::Equal => {
                        theirs.next();
                        ours.next();
                    }
                }
            }
        }
    }
}

/// The first cfg one of two packages is built with and the other not, of those that can change
/// the code of `at` of [`PACKAGES`] ([`compared`]).
fn cfg_difference(theirs: &[String], ours: &[String], at: usize) -> Option<String> {
    let set = |all: &[String]| -> FxHashSet<String> {
        all.iter().filter(|atom| compared(atom, at)).cloned().collect()
    };
    let (theirs, ours) = (set(theirs), set(ours));
    let mut more: Vec<&String> = theirs.difference(&ours).collect();
    more.sort();
    if let Some(atom) = more.first() {
        return Some(format!("it is built with `{atom}`, which the verified copy is not"));
    }
    let mut fewer: Vec<&String> = ours.difference(&theirs).collect();
    fewer.sort();
    fewer.first().map(|atom| format!("it is built without `{atom}`, which the verified copy is"))
}

/// Whether the cfg `atom` can change the code of `at` of [`PACKAGES`]: a feature, or one whose
/// name a source of it or of a package it depends on has.
fn compared(atom: &str, at: usize) -> bool {
    let name = atom.split_once('=').map_or(atom, |it| it.0);
    name == "feature" || mentioned(at, name)
}

/// Whether a file a library compiles from (its `build.rs` and the `.rs` files under `src/`,
/// which include no other, `tests.rs`), of `at` of [`PACKAGES`] or a package it depends on,
/// has `name` as a word.
fn mentioned(at: usize, name: &str) -> bool {
    static SEEN: Mutex<Option<FxHashMap<(usize, String), bool>>> = Mutex::new(None);
    let mut seen = lock(&SEEN);
    let seen = seen.get_or_insert_default();
    *seen.entry((at, name.to_owned())).or_insert_with(|| {
        closure_of(at).into_iter().any(|it| {
            PACKAGES[it]
                .files
                .iter()
                .filter(|(path, _)| compiled(path))
                .any(|(_, bytes)| has_word(bytes, name.as_bytes()))
        })
    })
}

/// Whether the library of a package compiles from its file at `path`.
fn compiled(path: &str) -> bool {
    path == "build.rs" || (path.starts_with("src/") && path.ends_with(".rs"))
}

fn has_word(bytes: &[u8], word: &[u8]) -> bool {
    let ident = |it: u8| it.is_ascii_alphanumeric() || it == b'_';
    !word.is_empty()
        && bytes.windows(word.len()).enumerate().any(|(at, it)| {
            it == word
                && (at == 0 || !ident(bytes[at - 1]))
                && bytes.get(at + word.len()).is_none_or(|it| !ident(*it))
        })
}

/// `at` of [`PACKAGES`] and every package it depends on, directly or not, once, in the order
/// they are first reached.
fn closure_of(at: usize) -> Vec<usize> {
    let mut all = vec![at];
    let mut next = 0;
    while let Some(&it) = all.get(next) {
        next += 1;
        for &(_, dep) in PACKAGES[it].deps {
            if !all.contains(&dep) {
                all.push(dep);
            }
        }
    }
    all
}

/// What an identity is made with.
const IDENTITY: &str = "blaze v2 native proc macro 1";

/// The code that runs a native expander, besides the packages it expands with.
const CODE: [&str; 8] = [
    include_str!("mod.rs"),
    include_str!("host.rs"),
    include_str!("wire.rs"),
    include_str!("lex.rs"),
    include_str!("verified.rs"),
    include_str!("files.rs"),
    include_str!("proc_macro/lib.rs"),
    include_str!("proc_macro/Cargo.toml"),
];

fn identity(root: usize, entry: usize) -> ContentId {
    static CLOSURES: OnceLock<Vec<ContentId>> = OnceLock::new();
    let closures = CLOSURES.get_or_init(|| {
        ROOTS.iter().map(|root| closure_identity(root.package)).collect()
    });
    let entry = &ROOTS[root].macros[entry];
    let kind: u8 = match entry.kind {
        ProcMacroKind::CustomDerive => 0,
        ProcMacroKind::Attr => 1,
        ProcMacroKind::Bang => 2,
    };
    Hasher::new(IDENTITY).id(&closures[root]).part(entry.name.as_bytes()).part(&[kind]).finish()
}

fn closure_identity(root: usize) -> ContentId {
    let actual = actual();
    let mut hasher = Hasher::new(IDENTITY);
    for code in CODE {
        hasher.part(code.as_bytes());
    }
    for at in closure_of(root) {
        let package = &PACKAGES[at];
        hasher.part(package.name.as_bytes()).part(package.version.as_bytes());
        for (path, hash) in &actual[at].files {
            hasher.part(path.as_bytes()).part(hash);
        }
        for atom in actual[at].cfg.as_deref().unwrap_or_default() {
            hasher.part(atom.as_bytes());
        }
        for (name, dep) in package.deps {
            hasher.part(name.as_bytes()).part(&(*dep as u64).to_le_bytes());
        }
    }
    hasher.finish()
}

/// What [`find`] found: the macros it gave expanders of, and each first difference of a
/// library of a verified crate's name and version from the copy, with how many had it.
struct Report {
    found: u64,
    missed: Vec<(String, u64)>,
}

static REPORT: Mutex<Report> = Mutex::new(Report { found: 0, missed: Vec::new() });

fn missed(why: String) {
    let mut report = lock(&REPORT);
    match report.missed.iter_mut().find(|it| it.0 == why) {
        Some(it) => it.1 += 1,
        None => report.missed.push((why, 1)),
    }
}

/// What [`find`] found so far, for the daemon's log: `None` before it found or missed one.
pub fn report() -> Option<String> {
    let report = lock(&REPORT);
    if report.found == 0 && report.missed.is_empty() {
        return None;
    }
    let mut said = format!("native proc macros: {} found", report.found);
    for (why, count) in &report.missed {
        said += &format!("; {count} not the verified copy, as {why}");
    }
    Some(said)
}

fn lock<T>(it: &Mutex<T>) -> MutexGuard<'_, T> {
    it.lock().unwrap_or_else(|it| it.into_inner())
}
