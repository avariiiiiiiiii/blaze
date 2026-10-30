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
//! What it costs: [`find`] of an unchanged library is a `stat` of its description and of each
//! file and directory of its packages, against columns kept from the last call ([`closure`]),
//! and a verdict kept with them: no read, no lock, no allocation. An expansion allocates only
//! what its output keeps: its spans' table and its encodings are each thread's, reused, and the
//! module's output is read straight into rust-analyzer's flat tree ([`wire::Reader`]).
//!
//! The packages are serde_derive's (`Serialize`, `Deserialize`), thiserror-impl's 1.x and
//! 2.x (`Error`) and tokio-macros' (`main`, `test` and the rest), over syn 2 and 3, quote,
//! proc-macro2 and unicode-ident (`docs/v2/plan/lanes/usercode/macros-native.md`). bitflags'
//! `bitflags!` has none: it is a `macro_rules!` macro, which the front end already expands
//! natively, and taking over more of it than its first step would drop the expansions whose
//! marks its output's syntax contexts carry.

mod closure;
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
    cell::Cell,
    cmp::Ordering,
    path::PathBuf,
    sync::{
        Mutex, MutexGuard, OnceLock,
        atomic::{self, AtomicU64},
    },
};

use base_db::{Env, SourceDatabase};
use blaze_ids::{ContentId, Hasher};
use hir_expand::proc_macro::{ProcMacroExpansionError, ProcMacroKind};
use paths::AbsPath;
use rustc_hash::FxHashSet;
use span::Span;
use tt::{Leaf, TokenTree, TopSubtree};

use self::{
    host::Host,
    verified::{Build, Expander, PACKAGES, ROOTS},
    wire::{Spans, Writer},
};

/// A proc-macro crate and every package it depends on but the standard library's, as
/// [`find`] holds it to the copy blaze verified.
#[derive(Clone, Debug)]
pub struct Closure {
    /// The macro's crate first, then every package it depends on, directly or not, once.
    pub packages: Vec<Package>,
    /// Whether it is a verified copy, worked out once.
    verdict: OnceLock<Verdict>,
}

impl Closure {
    pub fn new(packages: Vec<Package>) -> Closure {
        Closure { packages, verdict: OnceLock::new() }
    }

    fn verdict(&self) -> &Verdict {
        self.verdict.get_or_init(|| verdict_of(self))
    }
}

impl PartialEq for Closure {
    fn eq(&self, other: &Closure) -> bool {
        self.packages == other.packages
    }
}

impl Eq for Closure {}

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
    /// feature and what its build script set. Sorted, each once.
    pub cfg: Vec<String>,
    /// Each dependency: the name its code knows it by, and its index in
    /// [`Closure::packages`]. Sorted by name, each once.
    pub deps: Vec<(String, usize)>,
}

/// What a [`Closure`] is.
#[derive(Clone, Debug)]
enum Verdict {
    /// The verified copy whose crate is this of [`ROOTS`].
    Root(usize),
    /// None blaze verified.
    Other,
    /// Not a closure, for the reason given.
    Shape(String),
}

fn verdict_of(closure: &Closure) -> Verdict {
    if let Err(why) = shaped(closure) {
        return Verdict::Shape(why);
    }
    let crate_ = &closure.packages[0];
    let Some(root) = ROOTS.iter().position(|it| {
        let package = &PACKAGES[it.package];
        package.name == crate_.name && package.version == crate_.version
    }) else {
        return Verdict::Other;
    };
    match matches(closure, ROOTS[root].package) {
        Ok(()) => {
            VERIFIED.fetch_add(1, atomic::Ordering::Relaxed);
            Verdict::Root(root)
        }
        Err(why) => {
            missed(format!("{} {}: {why}", crate_.name, crate_.version));
            Verdict::Other
        }
    }
}

/// Where the description of the library of proc macros at a path is: it sets its second
/// argument to the path of the `rust-project.json` the library was built from and says `true`,
/// or says `false` when the library was built from none (then no native expander can be of
/// it), and `Err` when it can't tell.
pub type Describe = dyn Fn(&AbsPath, &mut PathBuf) -> Result<bool, String> + Send + Sync;

static DESCRIBE: OnceLock<Box<Describe>> = OnceLock::new();

/// Has [`find`] learn where each library's description is from `describe`, for every load
/// from here on. Says so when one was registered before.
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
    let Some(closure) = closure::of_library(dylib, &**describe)? else { return Ok(None) };
    find_in(&closure, dylib, name, kind)
}

fn find_in(
    closure: &Closure,
    dylib: &AbsPath,
    name: &str,
    kind: ProcMacroKind,
) -> Result<Option<Macro>, String> {
    let root = match closure.verdict() {
        Verdict::Root(root) => *root,
        Verdict::Other => return Ok(None),
        Verdict::Shape(why) => return Err(format!("the description of {dylib}: {why}")),
    };
    let entry =
        ROOTS[root].macros.iter().position(|it| it.name == name && it.kind == kind).ok_or_else(
            || {
                let crate_ = &PACKAGES[ROOTS[root].package];
                format!(
                    "{dylib} is the verified copy of {} {}, which declares no {kind:?} macro \
                     `{name}`",
                    crate_.name, crate_.version
                )
            },
        )?;
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

/// What an expansion on a thread writes and reads, kept for the next one there.
#[derive(Default)]
struct Scratch {
    spans: Spans,
    input: Vec<u8>,
    attr: Vec<u8>,
    output: Vec<u8>,
}

thread_local! {
    /// Taken for the length of an expansion: one that a database answer starts on this
    /// thread (`host.rs`) finds it empty and makes its own.
    static SCRATCH: Cell<Scratch> = Cell::new(Scratch::default());
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
        let mut scratch = SCRATCH.take();
        scratch.spans.clear();
        scratch.output.clear();
        // As the wasm host numbers them: the sites 1, 2 and 3, the same spans or not, then
        // the input's as they first appear.
        for span in [def_site, call_site, mixed_site] {
            scratch.spans.push(span);
        }
        {
            let Scratch { spans, input, attr, .. } = &mut scratch;
            input.clear();
            attr.clear();
            let mut writer = Writer { out: std::mem::take(input), spans };
            writer.top(subtree);
            *input = std::mem::take(&mut writer.out);
            match attrs {
                Some(attrs) => {
                    writer.out = std::mem::take(attr);
                    writer.top(attrs);
                    *attr = std::mem::take(&mut writer.out);
                }
                // rust-analyzer gives an attribute written without arguments
                // (`#[tokio::main]`) none, which the macro is given as an empty stream.
                None => attr.extend_from_slice(&0u32.to_le_bytes()),
            }
        }
        let mut host = Host { db, spans: std::mem::take(&mut scratch.spans), sites: [1, 2, 3] };
        let (input, attr, output) = (&scratch.input, &scratch.attr, &mut scratch.output);
        let ran = native_proc_macro::__native::with_host(&mut host, || match entry.expander {
            Expander::One(f) => native_proc_macro::__native::expand1(f, input, output),
            Expander::Two(f) => native_proc_macro::__native::expand2(f, attr, input, output),
        });
        let read = ran.map(|()| wire::Reader::new(&scratch.output, &host.spans).top(call_site));
        scratch.spans = host.spans;
        SCRATCH.set(scratch);
        match read {
            Ok(Ok(tree)) => Ok(tree),
            Ok(Err(e)) => Err(ProcMacroExpansionError::System(format!("`{}`: {e}", entry.name))),
            Err(panic) => Err(ProcMacroExpansionError::Panic(panic)),
        }
    }
}

/// The first difference of a native expander's output from the macro's, or `None` when they
/// are the same: every token tree, each delimiter, character, spacing, text, literal kind and
/// suffix, and of every span its range, anchor and syntax context.
pub fn difference(native: &TopSubtree, normal: &TopSubtree) -> Option<String> {
    let mut natively = native.as_token_trees().iter_flat_tokens();
    let mut normally = normal.as_token_trees().iter_flat_tokens();
    let mut at = 0usize;
    loop {
        match (natively.next(), normally.next()) {
            (None, None) => return None,
            (Some(ours), Some(theirs)) => {
                if let Some(what) = tree_difference(&ours, &theirs) {
                    return Some(format!("token tree {at}, {}: {what}", shown(&theirs)));
                }
            }
            (None, Some(theirs)) => {
                return Some(format!(
                    "the native output ends after {at} token trees, where the macro's goes on \
                     with {}",
                    shown(&theirs),
                ));
            }
            (Some(ours), None) => {
                return Some(format!(
                    "the native output goes on after the macro's {at} token trees with {}",
                    shown(&ours),
                ));
            }
        }
        at += 1;
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

/// Why `closure` is not one: a package it has not, or files, cfgs or dependencies out of
/// order.
fn shaped(closure: &Closure) -> Result<(), String> {
    if closure.packages.is_empty() {
        return Err("it is of no package".to_owned());
    }
    for package in &closure.packages {
        let named = || format!("{} {}", package.name, package.version);
        if let Some((name, at)) = package.deps.iter().find(|it| it.1 >= closure.packages.len()) {
            return Err(format!(
                "{} depends on `{name}`, its package {at}, which it has not",
                named()
            ));
        }
        if !package.files.windows(2).all(|it| it[0].0 < it[1].0) {
            return Err(format!("the files of {} are not sorted by path, each once", named()));
        }
        if !package.cfg.windows(2).all(|it| it[0] < it[1]) {
            return Err(format!("the cfg of {} is not sorted, each once", named()));
        }
        if !package.deps.windows(2).all(|it| it[0].0 < it[1].0) {
            return Err(format!("the dependencies of {} are not sorted by name, each once", named()));
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
/// package that differs, from the crate down its dependencies, and how. `closure` is
/// [`shaped`].
fn matches(closure: &Closure, root: usize) -> Result<(), String> {
    let actual = actual();
    // Of each package of `closure`, the package of `PACKAGES` it stands for, once reached; and
    // the pairs reached, in the order they are checked.
    let mut paired: Vec<Option<usize>> = vec![None; closure.packages.len()];
    paired[0] = Some(root);
    let mut pairs = Vec::with_capacity(closure.packages.len());
    pairs.push((0, root));
    let mut checked = 0;
    while let Some(&(at, ours)) = pairs.get(checked) {
        checked += 1;
        let theirs = &closure.packages[at];
        let package = &PACKAGES[ours];
        let named = || format!("{} {}", package.name, package.version);
        if theirs.name != package.name || theirs.version != package.version {
            return Err(format!(
                "{} {} is where the verified copy has {}",
                theirs.name,
                theirs.version,
                named()
            ));
        }
        if let Some(why) = files_difference(&theirs.files, &actual[ours].files) {
            return Err(format!("{}: {why}", named()));
        }
        let cfg = actual[ours].cfg.as_ref().map_err(|why| format!("{}: {why}", named()))?;
        if let Some(why) = cfg_difference(&theirs.cfg, cfg, ours) {
            return Err(format!("{}: {why}", named()));
        }
        let same_names = theirs.deps.len() == package.deps.len()
            && theirs.deps.iter().zip(package.deps).all(|(a, b)| a.0 == b.0);
        if !same_names {
            let names: Vec<&str> = theirs.deps.iter().map(|it| it.0.as_str()).collect();
            let verified: Vec<&str> = package.deps.iter().map(|it| it.0).collect();
            return Err(format!(
                "{} depends on [{}] where the verified copy depends on [{}]",
                named(),
                names.join(", "),
                verified.join(", "),
            ));
        }
        for (&(_, dep), &(_, verified_dep)) in theirs.deps.iter().zip(package.deps) {
            match paired[dep] {
                Some(it) if it != verified_dep => {
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
                    paired[dep] = Some(verified_dep);
                    pairs.push((dep, verified_dep));
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

/// The first cfg, in order, that one of two packages is built with and the other not, of those
/// that can change the code of `at` of [`PACKAGES`] ([`compared`]). Both are sorted.
fn cfg_difference(theirs: &[String], ours: &[String], at: usize) -> Option<String> {
    fn kept(all: &[String], at: usize) -> impl Iterator<Item = &str> + '_ {
        all.iter().map(String::as_str).filter(move |atom| compared(atom, at))
    }
    let (mut theirs, mut ours) = (kept(theirs, at).peekable(), kept(ours, at).peekable());
    let more = |atom: &str| format!("it is built with `{atom}`, which the verified copy is not");
    let fewer = |atom: &str| format!("it is built without `{atom}`, which the verified copy is");
    loop {
        match (theirs.peek().copied(), ours.peek().copied()) {
            (None, None) => return None,
            (Some(atom), None) => return Some(more(atom)),
            (None, Some(atom)) => return Some(fewer(atom)),
            (Some(a), Some(b)) => match a.cmp(b) {
                Ordering::Less => return Some(more(a)),
                Ordering::Greater => return Some(fewer(b)),
                Ordering::Equal => {
                    theirs.next();
                    ours.next();
                }
            },
        }
    }
}

/// Whether the cfg `atom` can change the code of `at` of [`PACKAGES`]: a feature, or one whose
/// name a source of it or of a package it depends on has.
fn compared(atom: &str, at: usize) -> bool {
    let name = atom.split_once('=').map_or(atom, |it| it.0);
    name == "feature" || mentioned(at, name)
}

/// Whether a file a library compiles from (its `build.rs` and the `.rs` files under `src/`,
/// which include no other, `tests.rs`), of `at` of [`PACKAGES`] or a package it depends on,
/// has `name` as an identifier. One that is not all identifier characters is taken as named.
fn mentioned(at: usize, name: &str) -> bool {
    if name.is_empty() || !name.bytes().all(identifier) {
        return true;
    }
    let words = words();
    closure_of(at).iter().any(|&it| words[it].contains(name.as_bytes()))
}

/// Whether the library of a package compiles from its file at `path`.
fn compiled(path: &str) -> bool {
    path == "build.rs" || (path.starts_with("src/") && path.ends_with(".rs"))
}

fn identifier(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}

/// Of each package of [`PACKAGES`], every longest run of identifier characters in the files it
/// compiles from, each a slice of the file: made once, then a cfg name is one probe of a set.
fn words() -> &'static [FxHashSet<&'static [u8]>] {
    static WORDS: OnceLock<Vec<FxHashSet<&'static [u8]>>> = OnceLock::new();
    WORDS.get_or_init(|| {
        PACKAGES
            .iter()
            .map(|package| {
                let mut words = FxHashSet::default();
                for &(path, bytes) in package.files {
                    if !compiled(path) {
                        continue;
                    }
                    let mut start = None;
                    for (at, &byte) in bytes.iter().enumerate() {
                        match (identifier(byte), start) {
                            (true, None) => start = Some(at),
                            (false, Some(from)) => {
                                words.insert(&bytes[from..at]);
                                start = None;
                            }
                            _ => {}
                        }
                    }
                    if let Some(from) = start {
                        words.insert(&bytes[from..]);
                    }
                }
                words
            })
            .collect()
    })
}

/// `at` of [`PACKAGES`] and every package it depends on, directly or not, once, in the order
/// they are first reached.
fn closure_of(at: usize) -> &'static [usize] {
    static CLOSURES: OnceLock<Vec<Vec<usize>>> = OnceLock::new();
    &CLOSURES.get_or_init(|| {
        (0..PACKAGES.len())
            .map(|at| {
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
            })
            .collect()
    })[at]
}

/// What an identity is made with.
const IDENTITY: &str = "blaze v2 native proc macro 1";

/// The code that runs a native expander, besides the packages it expands with.
const CODE: [&str; 9] = [
    include_str!("mod.rs"),
    include_str!("closure.rs"),
    include_str!("host.rs"),
    include_str!("wire.rs"),
    include_str!("lex.rs"),
    include_str!("verified.rs"),
    include_str!("files.rs"),
    include_str!("proc_macro/lib.rs"),
    include_str!("proc_macro/Cargo.toml"),
];

/// The identity of macro `entry` of `root` of [`ROOTS`], each worked out once.
fn identity(root: usize, entry: usize) -> ContentId {
    static IDENTITIES: OnceLock<Vec<Vec<ContentId>>> = OnceLock::new();
    IDENTITIES.get_or_init(|| {
        ROOTS
            .iter()
            .map(|root| {
                let closure = closure_identity(root.package);
                root.macros
                    .iter()
                    .map(|entry| {
                        let kind: u8 = match entry.kind {
                            ProcMacroKind::CustomDerive => 0,
                            ProcMacroKind::Attr => 1,
                            ProcMacroKind::Bang => 2,
                        };
                        Hasher::new(IDENTITY)
                            .id(&closure)
                            .part(entry.name.as_bytes())
                            .part(&[kind])
                            .finish()
                    })
                    .collect()
            })
            .collect()
    })[root][entry]
}

fn closure_identity(root: usize) -> ContentId {
    let actual = actual();
    let mut hasher = Hasher::new(IDENTITY);
    for code in CODE {
        hasher.part(code.as_bytes());
    }
    for &at in closure_of(root) {
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

/// How many closures were the verified copy, each counted once when it is first asked of.
static VERIFIED: AtomicU64 = AtomicU64::new(0);

/// Each first difference of a closure of a verified crate's name and version from the copy,
/// with how many closures had it: written only on a miss.
static MISSED: Mutex<Vec<(String, u64)>> = Mutex::new(Vec::new());

fn missed(why: String) {
    let mut missed = lock(&MISSED);
    match missed.iter_mut().find(|it| it.0 == why) {
        Some(it) => it.1 += 1,
        None => missed.push((why, 1)),
    }
}

/// What [`find`] found so far, for the daemon's log: `None` before it found or missed one.
pub fn report() -> Option<String> {
    let verified = VERIFIED.load(atomic::Ordering::Relaxed);
    let missed = lock(&MISSED);
    if verified == 0 && missed.is_empty() {
        return None;
    }
    let mut said = format!("native proc macros: {verified} libraries are verified copies");
    for (why, count) in missed.iter() {
        said += &format!("; {count} not the verified copy, as {why}");
    }
    Some(said)
}

fn lock<T>(it: &Mutex<T>) -> MutexGuard<'_, T> {
    it.lock().unwrap_or_else(|it| it.into_inner())
}
