//! Proc-macro expansions kept in the store (docs/v2/plan/lanes/usercode.md), so that no
//! macro is expanded twice for the same inputs, on this machine or, through the store's
//! remote, another.
//!
//! An expansion is a run of user code ([`blaze_usercode::RunKey`]): its code is the content ID
//! of the macro's library, and what it is given ([`given`]) is the macro's name and kind, its
//! input and attribute tokens, the variables of the crate it expands for, the directory it
//! runs in, the toolchain and the proc-macro ABI. Spans are given relative: each as its byte
//! range from the item it is anchored to and which of the call's anchors and syntax contexts
//! it has, numbered as they first appear, so the next call with the same input in another
//! database or revision has the same key. The value is the output with its spans written the
//! same way. What else it read (files, variables of the shell, a listing) its server's
//! sandbox traced ([`proc_macro_api::warm`]), and the result is found and kept by exactly
//! those through [`blaze_usercode::index`]. Every kind of macro is kept.
//!
//! Not kept: an expansion that asked the database about a span (its text, file or line),
//! since the key does not say what it was told; one whose output has an anchor or a context
//! its input has not, which the next call could not be given back; one that read what can't
//! be traced (the clock, randomness); one its server could not trace (`BLAZE_SANDBOX=off`);
//! and one that failed. `BLAZE_PROC_MACRO_CACHE=check` expands every call it finds kept
//! anyway, and an output other than the kept one is an error naming the macro.
//!
//! An expansion runs inside a query on a worker, many at once, so it writes nothing shared: it
//! reads the store, and what it keeps it leaves in its worker's own buffer for the store's
//! owner ([`puts`]); its statistics are its worker's ([`tally`]).
//!
//! [`native`] is blaze's in-process proc-macro bridge, the `proc_macro` a macro's own code runs
//! against and the host that answers it, for the proc macros that run on blaze's own compiler
//! (docs/v2/plan/design/compile-time.md, change 20, lane fastcompile). Nothing here calls it.
//!
//! [`compare`] holds the toolchain's server against blaze's, and keeps nothing while it does.

pub mod bridge;
mod compare;
pub mod native;
pub mod puts;
pub mod tally;
pub mod verdict;

use std::{
    any::Any,
    cell::Cell,
    sync::{
        Arc, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use base_db::{Db, Env};
use blaze_cas::Reader;
use blaze_core::Traced;
use blaze_ids::ContentId;
use blaze_usercode::{
    Given, RunKey, RunKind,
    index::{self, FindError, Found},
    inputs::{self, Files, Now},
    trace::Inputs,
};

pub use puts::{Published, Put, publish, put, take_puts};
use hir_expand::proc_macro::{
    ExpansionReads, ProcMacroCall, ProcMacroExpander, ProcMacroExpansionError, ProcMacroKind, report_reads,
};
use bridge::wire::{self, Parts};
use paths::{AbsPath, AbsPathBuf};
use proc_macro_api::{
    version::CURRENT_API_VERSION,
    warm::{self, Environment},
};
use rustc_hash::FxHashMap;
use span::{Span, SpanAnchor, SyntaxContext};

/// What the content ID of a macro's library is made with.
pub const LIBRARY: &str = "blaze v2 proc-macro library 1";

/// The variable that says to check kept expansions.
pub const CACHE: &str = "BLAZE_PROC_MACRO_CACHE";
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// What is found is used.
    Keep,
    /// What is found is expanded again and compared.
    Check,
}

impl Mode {
    /// The mode `value` of [`CACHE`] says.
    pub fn of(value: Option<&str>) -> Result<Mode, String> {
        match value {
            None => Ok(Mode::Keep),
            Some("check") => Ok(Mode::Check),
            Some(other) => Err(format!( // alloc: error path
                "{CACHE}={other:?} is not a mode: every expansion that can be kept is, and \
                 `check` expands what was kept again and compares"
            )),
        }
    }
}

/// Where expansions are kept, and what they are keyed by besides their call.
pub struct Expansions {
    /// Read to find an expansion, with no lock, under a guard taken for that read alone;
    /// never written by one: what an expansion keeps goes to its worker's buffer
    /// ([`take_puts`]), which the store's owner [`publish`]es.
    pub store: Reader,
    /// The toolchain whose proc-macro ABI and library the macros are built with.
    pub toolchain: ContentId,
    /// What warm servers pass through to the expansions, which they are checked against.
    pub environment: Arc<Environment>,
}

struct Installed {
    expansions: Expansions,
    mode: Mode,
}

static INSTALLED: OnceLock<Installed> = OnceLock::new();

/// The inputs of a server's expansion as they are now: the machine's files, and the
/// environment it is sent ([`Environment::seen`]). A variable a shape names is looked up where
/// it is, so a hit allocates nothing; the whole environment is worked out only for a run that
/// read all of it.
struct Seen<'a> {
    passed: &'a [(String, String)],
    call: &'a Env,
    made: Option<inputs::Env>,
    /// What file, directory and listing reads are asked through: no variable is read there.
    none: inputs::Env,
}

impl<'a> Seen<'a> {
    /// The value [`Environment::seen`] gives `name`: the call's, over what is passed, over
    /// what blaze sets for a server ([`warm::SET`]). A name that is not UTF-8 is none of them.
    fn value(&self, name: &std::ffi::OsStr) -> Option<&'a str> {
        let name = name.to_str()?;
        if let Some((_, value)) = self.call.iter().find(|it| it.0 == name) {
            return Some(value);
        }
        if let Some((_, value)) = self.passed.iter().rev().find(|it| it.0 == name) {
            return Some(value.as_str());
        }
        warm::SET.iter().find(|it| it.0 == name).map(|it| it.1)
    }
}

impl Inputs for Seen<'_> {
    type Error = inputs::Unreadable;
    fn entry(&mut self, path: &std::path::Path) -> Result<blaze_usercode::trace::Entry, Self::Error> {
        Now { files: Files::shared(), env: &self.none }.entry(path)
    }

    fn file(&mut self, path: &std::path::Path) -> Result<Option<[u8; 32]>, Self::Error> {
        Now { files: Files::shared(), env: &self.none }.file(path)
    }

    fn dir(&mut self, path: &std::path::Path) -> Result<Option<[u8; 32]>, Self::Error> {
        Now { files: Files::shared(), env: &self.none }.dir(path)
    }

    fn env(&mut self, name: &std::ffi::OsStr) -> Option<&std::ffi::OsStr> {
        self.value(name).map(std::ffi::OsStr::new)
    }

    fn env_all(&mut self) -> &[(std::ffi::OsString, std::ffi::OsString)] {
        let (passed, call) = (self.passed, self.call);
        // alloc: a run that read the whole environment: it is sorted once per call, as its key needs.
        self.made.get_or_insert_with(|| inputs::Env::new(Environment::seen(passed, call.iter()))).vars()
    }

    fn names(
        &mut self,
        path: &std::path::Path,
        out: &mut blaze_usercode::trace::Names<'_>,
    ) -> Result<bool, Self::Error> {
        Now { files: Files::shared(), env: &self.none }.names(path, out)
    }
}

/// What `check` compares a kept expansion by, read in place: its bytes' hash.
pub fn checked(bytes: &[u8]) -> ContentId {
    ContentId::of("blaze v2 proc-macro checked expansion 1", bytes)
}

/// What the store has of a call, each with what its expansion read.
enum Lookup {
    /// Its expansion, read back.
    Read(tt::TopSubtree, ExpansionReads),
    /// The hash of its expansion's bytes, to compare a fresh one with ([`Mode::Check`]).
    Hashed(ContentId, ExpansionReads),
    Nothing,
}

/// Keeps every proc-macro expansion of the workspaces loaded from here on in `expansions`.
/// Says why not when [`CACHE`] has a value that is not a mode, or it was installed before.
pub fn install(expansions: Expansions) -> Result<(), String> {
    let var = |name: &str| match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!("{name} is not UTF-8")), // alloc: error path
    };
    let mode = Mode::of(var(CACHE)?.as_deref())?;
    wrapping()?;
    // alloc: error path
    INSTALLED.set(Installed { expansions, mode }).map_err(|_| "proc-macro expansions were installed twice".to_owned())?;
    load_cargo::wrap_expanders(wrap);
    Ok(())
}

/// Stops every proc-macro expansion of the workspaces loaded from here on that failed for a
/// reason that is not the macro's own ([`server_failed`]), as [`install`] does, for a process
/// with no store to keep them in (the engine): nothing is kept or looked for, and
/// [`keeping`] is `None`. Says why not when expansions were installed before.
pub fn install_unkept() -> Result<(), String> {
    wrapping()?;
    load_cargo::wrap_expanders(wrap_unkept);
    Ok(())
}

/// Takes the one wrapping of the process's expanders: `load_cargo` keeps the first silently,
/// so a second, of either kind, is an error here.
fn wrapping() -> Result<(), String> {
    static WRAPPED: AtomicBool = AtomicBool::new(false);
    match WRAPPED.swap(true, Ordering::AcqRel) {
        false => Ok(()),
        true => Err("proc-macro expansions were installed twice".to_owned()), // alloc: error path
    }
}

fn installed() -> &'static Installed {
    INSTALLED.get().expect("an expander is wrapped only once install has been")
}

/// Where an expander that keys its own expansions (blaze-macro-wasm's) keeps them, and how.
#[derive(Clone, Copy)]
pub struct Keeping {
    pub expansions: &'static Expansions,
    pub mode: Mode,
}

/// Where expansions are kept: `None` before [`install`], as in a bench, which keeps nothing.
pub fn keeping() -> Option<Keeping> {
    INSTALLED.get().map(|it| Keeping { expansions: &it.expansions, mode: it.mode })
}

/// The hash of the values the names of `shape` (an [`ExpansionReads::Traced`] shape) have
/// now, for an expansion given the variables `env`: what `hir_def::deps` files a crate's
/// image by. The same hash the expansion reported when its values were these.
pub fn reads_now(env: &Env, shape: &[u8]) -> Result<ContentId, String> {
    reads_in(installed().expansions.environment.passed(), env, shape)
}

/// [`reads_now`] for a server passed `passed`.
fn reads_in(passed: &[(String, String)], env: &Env, shape: &[u8]) -> Result<ContentId, String> {
    let mut now = Seen { passed, call: env, made: None, none: inputs::Env::default() };
    match index::values(shape, &mut now) {
        Ok(Some(values)) => Ok(values),
        // A directory that is no longer one: no value it had is this.
        Ok(None) => Ok(ContentId::of("blaze v2 expansion reads gone", shape)),
        Err(FindError::NotAShape { why, .. }) => Err(format!("a shape that does not read: {why}")), // alloc: error path
        Err(FindError::Inputs(unreadable)) => Err(format!("{unreadable}")), // alloc: error path
        Err(FindError::Corrupt(corrupt)) => Err(format!("{corrupt}")), // alloc: error path
    }
}

fn wrap_unkept(
    inner: Arc<dyn ProcMacroExpander>, // dyn: the database's expander, the edge, wrapped once per macro
    dylib: &AbsPath,
    name: &str,
    _: ProcMacroKind,
) -> Arc<dyn ProcMacroExpander> { // dyn: as above
    Arc::new(Unkept { inner, dylib: dylib.to_path_buf(), name: name.into() }) // alloc: setup, once per macro loaded
}

/// An expander whose expansions are not kept ([`install_unkept`]): only a failure that is not
/// the macro's own is told apart, as [`Kept`] tells it.
#[derive(Debug)]
struct Unkept {
    inner: Arc<dyn ProcMacroExpander>, // dyn: the database's expander, the edge (`wrap_unkept`)
    dylib: AbsPathBuf,
    name: Box<str>,
}

impl ProcMacroExpander for Unkept {
    fn requires_session(&self) -> bool {
        self.inner.requires_session()
    }

    fn code(&self) -> Option<ContentId> {
        self.inner.code()
    }

    fn expand(&self, db: &Db, call: ProcMacroCall<'_>) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        failed(&self.name, &self.dylib, self.inner.expand(db, call))
    }

    fn expand_in(
        &self,
        db: &Db,
        calling_crate: base_db::Crate,
        call: ProcMacroCall<'_>,
    ) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        failed(&self.name, &self.dylib, self.inner.expand_in(db, calling_crate, call))
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool { // dyn: the database's expander trait, the edge
        (other as &dyn Any) // dyn: as above, told apart by type
            .downcast_ref::<Unkept>()
            .is_some_and(|other| self.name == other.name && self.inner.eq_dyn(&*other.inner))
    }
}

/// `expanded`, or the failure of the server of macro `name` of `dylib`, said once
/// ([`server_failed`]), when it failed for a reason that is not the macro's own.
fn failed(
    name: &str,
    dylib: &AbsPath,
    expanded: Result<tt::TopSubtree, ProcMacroExpansionError>,
) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
    match expanded {
        Err(ProcMacroExpansionError::System(why)) => Err(system(format!( // alloc: error path
            "the proc-macro server failed to expand `{name}` of {dylib}: {why}; what it did not \
             expand is missing from the program, so nothing is built from it"
        ))),
        other => other,
    }
}

fn wrap(
    inner: Arc<dyn ProcMacroExpander>, // dyn: the database's expander, the edge, wrapped once per macro
    dylib: &AbsPath,
    name: &str,
    kind: ProcMacroKind,
) -> Arc<dyn ProcMacroExpander> { // dyn: as above
    Arc::new(Kept { inner, dylib: Dylib::at(dylib), name: name.into(), kind }) // alloc: setup, once per macro loaded
}

/// A library of proc macros, hashed the first time one of its macros expands.
#[derive(Debug)]
struct Dylib {
    path: AbsPathBuf,
    code: OnceLock<Result<ContentId, String>>,
}

impl Dylib {
    /// The one library at `path` as it is now: the macros of a library share its hash, and a
    /// library built again where it was is another.
    fn at(path: &AbsPath) -> Arc<Dylib> {
        type Seen = (AbsPathBuf, Option<(SystemTime, u64)>);
        static DYLIBS: Mutex<Option<FxHashMap<Seen, Arc<Dylib>>>> = Mutex::new(None);
        let stamp = std::fs::metadata(path).ok().and_then(|it| Some((it.modified().ok()?, it.len())));
        let mut dylibs = DYLIBS.lock().unwrap_or_else(|it| it.into_inner());
        let dylibs = dylibs.get_or_insert_default();
        dylibs
            .entry((path.to_path_buf(), stamp))
            // alloc: setup, once per library and macro as a workspace loads
            .or_insert_with(|| Arc::new(Dylib { path: path.to_path_buf(), code: OnceLock::new() }))
            .clone() // clone: setup, the macro's handle on its library
    }

    /// The library's content ID, hashed once; why it can't be read, lent, when it can't.
    fn code(&self) -> Result<ContentId, &str> {
        let code = || match std::fs::read(&self.path) {
            Ok(bytes) => Ok(ContentId::of(LIBRARY, &bytes)),
            // alloc: error path
            Err(error) => Err(format!("the proc-macro library {} can't be read: {error}", self.path)),
        };
        self.code.get_or_init(code).as_ref().copied().map_err(String::as_str)
    }
}

#[derive(Debug)]
struct Kept {
    inner: Arc<dyn ProcMacroExpander>, // dyn: the database's expander, the edge (`wrap`)
    dylib: Arc<Dylib>,
    name: Box<str>,
    kind: ProcMacroKind,
}

/// The anchors and syntax contexts of a call's span parts, in the order they first appear: what
/// its output is written against. Its room goes back to the thread's shelf when it is dropped,
/// so the next call's takes no new memory.
#[derive(Default, Debug)]
pub struct Spans {
    anchors: Vec<SpanAnchor>,
    contexts: Vec<SyntaxContext>,
}

thread_local! {
    /// The room of the last [`Spans`] dropped on this thread.
    // alloc: none, an empty shelf; what it keeps is grown by the calls.
    static SHELF: Cell<Option<(Vec<SpanAnchor>, Vec<SyntaxContext>)>> = const { Cell::new(None) };
    /// What a key, a kept value and its reading reuse: taken for one of them, so a call made
    /// inside another (an expansion answering a sub-request) gets fresh ones.
    static SCRATCH: Cell<Option<Box<Scratch>>> = const { Cell::new(None) };
}

impl Spans {
    fn shelved() -> Spans {
        let (mut anchors, mut contexts) = SHELF.take().unwrap_or_default();
        anchors.clear();
        contexts.clear();
        Spans { anchors, contexts }
    }
}

impl Drop for Spans {
    fn drop(&mut self) {
        let room = (std::mem::take(&mut self.anchors), std::mem::take(&mut self.contexts));
        // A thread that is ending keeps nothing.
        _ = SHELF.try_with(|shelf| shelf.set(Some(room)));
    }
}

/// A call's span parts, its trees' relative columns and their shapes, and the room a kept tree
/// off a word is copied into: each as long as the longest it held.
#[derive(Default)]
struct Scratch {
    parts: Parts,
    input: Vec<u8>,
    attrs: Vec<u8>,
    shapes: Vec<u8>,
    room: Vec<u8>,
}

fn with_scratch<R>(f: impl FnOnce(&mut Scratch) -> R) -> R {
    // alloc: the first call on a thread, or one inside another, makes its room.
    let mut scratch = SCRATCH.take().unwrap_or_default();
    let done = f(&mut scratch);
    SCRATCH.set(Some(scratch));
    done
}

/// The number of `it` among `all`, in the order first seen: a new one is pushed. A scan: a
/// call's anchors and contexts are few.
pub fn number<T: PartialEq>(all: &mut Vec<T>, it: T) -> u32 {
    let at = all.iter().position(|known| *known == it).unwrap_or_else(|| {
        all.push(it);
        all.len() - 1
    });
    at as u32
}

/// One call of a macro, as its key is made of it.
pub struct Call<'a> {
    pub name: &'a str,
    pub kind: ProcMacroKind,
    pub subtree: &'a tt::TopSubtree,
    pub attrs: Option<&'a tt::TopSubtree>,
    pub env: &'a Env,
    pub def_site: Span,
    pub call_site: Span,
    pub mixed_site: Span,
    pub current_dir: &'a str,
}

fn kind_byte(kind: ProcMacroKind) -> u8 {
    match kind {
        ProcMacroKind::CustomDerive => 0,
        ProcMacroKind::Attr => 1,
        ProcMacroKind::Bang => 2,
    }
}

/// What `call` gives the macro, for its [`RunKey::given`] with the toolchain `toolchain`, and
/// the anchors and contexts its output is written against. A wasm macro's caller adds what
/// else it gives a module.
///
/// The trees are hashed as their own columns made relative ([`wire::relative`]: each span
/// part an index of the call's, the def, call and mixed sites' first), and the parts as the
/// numbers of their anchors and contexts in the order they first appear, then each site's
/// range and part, so the same tokens in another file or item have the same key. One pass over
/// each tree's columns, into buffers kept per thread: a key allocates nothing.
pub fn given(toolchain: &ContentId, call: &Call<'_>) -> (Given, Spans) {
    let mut spans = Spans::shelved();
    let mut given = Given::new(RunKind::ProcMacro);
    given
        .part("name", call.name.as_bytes())
        .part("kind", &[kind_byte(call.kind)])
        .part("abi", &CURRENT_API_VERSION.to_le_bytes())
        .id("toolchain", toolchain)
        .part("dir", call.current_dir.as_bytes());
    with_scratch(|scratch| {
        let Scratch { parts, input, attrs, shapes, .. } = scratch;
        parts.clear();
        input.clear();
        attrs.clear();
        shapes.clear();
        let sites = [call.def_site, call.call_site, call.mixed_site].map(|site| {
            let part = parts.part(site.anchor, site.ctx);
            [site.range.start().into(), site.range.len().into(), part]
        });
        wire::relative(call.subtree, parts, input);
        if let Some(it) = call.attrs {
            wire::relative(it, parts, attrs);
        }
        for &(anchor, ctx) in parts.all() {
            for it in [number(&mut spans.anchors, anchor), number(&mut spans.contexts, ctx)] {
                shapes.extend_from_slice(&it.to_le_bytes());
            }
        }
        for it in sites.as_flattened() {
            shapes.extend_from_slice(&it.to_le_bytes());
        }
        let attrs: &[u8] = if call.attrs.is_some() { attrs } else { b"-" };
        given
            .part("input columns", input.as_slice())
            .part("attrs columns", attrs)
            .part("span parts", shapes.as_slice());
    });
    given.env_keyed(call.env.iter().map(|(name, value)| (name.as_bytes(), value.as_bytes())));
    (given, spans)
}

/// The output as it is kept: its columns made relative ([`wire::relative`]), each span part an
/// index of the output's own, then the numbers of each part's anchor and context among the
/// call's `spans` (two words), then how many parts there are. `None` when it has an anchor or
/// a context the call has not. The tree is first, so the value's start is its start.
pub fn written(output: &tt::TopSubtree, spans: &Spans) -> Option<Vec<u8>> {
    with_scratch(|scratch| {
        let parts = &mut scratch.parts;
        parts.clear();
        // alloc: the value kept, once as an expansion is kept.
        let mut value = Vec::new();
        wire::relative(output, parts, &mut value);
        for &(anchor, ctx) in parts.all() {
            let anchor = spans.anchors.iter().position(|it| *it == anchor)? as u32;
            let context = spans.contexts.iter().position(|it| *it == ctx)? as u32;
            for it in [anchor, context] {
                value.extend_from_slice(&it.to_le_bytes());
            }
        }
        value.extend_from_slice(&(parts.len() as u32).to_le_bytes());
        Some(value)
    })
}

/// What [`written`] wrote, against the call's `spans`: one checked pass over the columns into
/// the tree, its one allocation, with a tree off a word copied into the thread's room.
pub fn read(bytes: &[u8], spans: &Spans) -> Result<tt::TopSubtree, String> {
    // alloc: error path.
    let cut = || "not a kept expansion: its table of span parts is cut short".to_owned();
    let (rest, count) = bytes.split_last_chunk::<4>().ok_or_else(cut)?;
    let table = (u32::from_le_bytes(*count) as usize)
        .checked_mul(8)
        .filter(|&len| len <= rest.len())
        .ok_or_else(cut)?;
    let (tree, table) = rest.split_at(rest.len() - table);
    with_scratch(|scratch| {
        let Scratch { parts, room, .. } = scratch;
        parts.clear();
        for (at, row) in table.as_chunks::<8>().0.iter().enumerate() {
            let field = |i: usize| u32::from_le_bytes(row[i..i + 4].try_into().expect("four bytes"));
            let anchor = spans.anchors.get(field(0) as usize);
            let ctx = spans.contexts.get(field(4) as usize);
            let Some((&anchor, &ctx)) = anchor.zip(ctx) else {
                // alloc: error path.
                return Err(format!("a kept expansion's span part {at} is of an anchor or context its call has not"));
            };
            if parts.part(anchor, ctx) as usize != at {
                // alloc: error path.
                return Err(format!("a kept expansion's span part {at} is an earlier one again"));
            }
        }
        // alloc: error path.
        wire::read_in(tree, parts, room).map_err(|error| format!("a kept expansion: {error}"))
    })
}

/// The first expansion that failed for a reason that is not the macro's own (its server died,
/// the store is broken): what is said of it.
static SERVER_FAILED: OnceLock<String> = OnceLock::new();

fn system(said: String) -> ProcMacroExpansionError {
    if SERVER_FAILED.set(said.clone()).is_ok() { // clone: error path, said once and returned
        eprintln!("proc macros: {said}");
    }
    ProcMacroExpansionError::System(said)
}

/// What is said of the first expansion in this process that failed for a reason that is not
/// the macro's own, when one did. The macros of that call did not expand, so what is built
/// from them is not the program: a build fails with this, at the point it is first seen,
/// rather than with a name the expansion would have defined (no-fallback).
pub fn server_failed() -> Option<&'static str> {
    SERVER_FAILED.get().map(String::as_str)
}

impl Kept {
    fn failed(
        &self,
        expanded: Result<tt::TopSubtree, ProcMacroExpansionError>,
    ) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        failed(&self.name, &self.dylib.path, expanded)
    }

    fn error(&self, why: impl std::fmt::Display) -> ProcMacroExpansionError {
        system(format!("{}: {why}", self.what())) // alloc: error path
    }

    /// Expands by the macro's server or module, through the store, and reports what the
    /// expansion read ([`report_reads`]) on every path that returns it.
    fn kept(&self, db: &Db, call: ProcMacroCall<'_>) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        let installed = installed();
        let start = Instant::now();
        let keyed = Call {
            name: &self.name,
            kind: self.kind,
            subtree: call.subtree,
            attrs: call.attrs,
            env: call.env,
            def_site: call.def_site,
            call_site: call.call_site,
            mixed_site: call.mixed_site,
            current_dir: call.current_dir,
        };
        let (given, spans) = given(&installed.expansions.toolchain, &keyed);
        let code = self.dylib.code().map_err(|why| self.error(why))?;
        let run = RunKey { code, given: given.finish() };
        // The files' table and this call's environment, made only if a shape read a variable:
        // no lock, and nothing here asks the database, so the store's guard is the one thing
        // held across `find_reads`.
        let mut now = Seen {
            passed: installed.expansions.environment.passed(),
            call: call.env,
            made: None,
            none: inputs::Env::default(),
        };
        let found = {
            let guard = blaze_cas::enter();
            match index::find_reads(&installed.expansions.store, &guard, &run, &mut now) {
                // Read in place, under the guard: a hit copies nothing out of the store but
                // the shape it reports, which the calling crate's query owns.
                Ok(Found::Hit { value, shape, values: Some(values), .. }) => {
                    let reads = ExpansionReads::Traced { shape: shape.into(), values }; // alloc: what the query reporting it owns
                    match installed.mode {
                        Mode::Keep => read(value, &spans)
                            .map(|output| Lookup::Read(output, reads))
                            .map_err(|why| self.error(why)),
                        Mode::Check => Ok(Lookup::Hashed(checked(value), reads)),
                    }
                }
                Ok(Found::Hit { values: None, .. }) => Err(self.error("the store's lookup found a hit without its values")),
                Ok(Found::Miss { .. }) => Ok(Lookup::Nothing),
                Err(FindError::Corrupt(corrupt)) => Err(self.error(corrupt)),
                Err(FindError::NotAShape { key, why }) => {
                    Err(self.error(format!("the store's {key:?} is not a shape: {why}"))) // alloc: error path
                }
                Err(FindError::Inputs(unreadable)) => Err(self.error(unreadable)),
            }
        }?;
        count(Counted::Keyed, start.elapsed());
        let found = match found {
            Lookup::Read(output, reads) => {
                report_reads(reads);
                count(Counted::Found, start.elapsed());
                return Ok(output);
            }
            Lookup::Hashed(hash, reads) => Some((hash, reads)),
            Lookup::Nothing => None,
        };
        let asked = load_cargo::asked();
        let expand = Instant::now();
        let expanded = self.inner.expand(db, call);
        let traced = warm::take_traced();
        count(Counted::Expanded, expand.elapsed());
        count(Counted::of(self.kind), expand.elapsed());
        let output = match self.failed(expanded) {
            Ok(output) => output,
            Err(error) => {
                count(Counted::Failed, Duration::ZERO);
                return Err(error);
            }
        };
        let bytes = written(&output, &spans);
        if let Some((kept, reads)) = found {
            if bytes.as_deref().map(checked) != Some(kept) {
                return Err(self.error(format!( // alloc: error path
                    "{CACHE}=check: it expanded otherwise than the store kept it, under a key \
                     of every input it was traced to read"
                )));
            }
            report_reads(reads);
            count(Counted::Checked, Duration::ZERO);
            return Ok(output);
        }
        let traced = match self.keepable(traced, asked) {
            Ok(Some(traced)) => traced,
            Ok(None) => return Ok(output),
            Err(error) => return Err(error),
        };
        report_reads(ExpansionReads::Traced {
            shape: index::Shape::of(&traced).encode().into(), // alloc: once, as an expansion is kept
            values: index::VALUES.traced(&traced),
        });
        let Some(bytes) = bytes else {
            count(Counted::Unspanned, Duration::ZERO);
            return Ok(output);
        };
        put(Put { run, traced, value: bytes, what: self.what() });
        count(Counted::Kept, Duration::ZERO);
        Ok(output)
    }

    /// This macro, as an error about one of its expansions begins.
    fn what(&self) -> String {
        format!("the expansion of `{}` of {}", self.name, self.dylib.path) // alloc: an error, or what a put said owns
    }

    /// What the expansion read, when it can be kept by it: `None` when it can't be, counted
    /// by why and reported as not known ([`ExpansionReads::Unknown`]).
    fn keepable(
        &self,
        traced: Option<Result<Traced, String>>,
        asked: u64,
    ) -> Result<Option<Traced>, ProcMacroExpansionError> {
        let unknown = |counted: Counted, why: &'static str| {
            count(counted, Duration::ZERO);
            report_reads(ExpansionReads::Unknown(why));
            Ok(None)
        };
        let traced = match traced {
            Some(Ok(traced)) => traced,
            Some(Err(why)) => return Err(self.error(why)),
            None => return unknown(Counted::Untraced, "its server did not trace it"),
        };
        if load_cargo::asked() != asked {
            return unknown(Counted::Asked, "it asked the database");
        }
        if index::untraceable(&traced) {
            return unknown(Counted::Untraceable, "it read what can't be traced (the clock, a socket, a process)");
        }
        Ok(Some(traced))
    }
}

impl ProcMacroExpander for Kept {
    fn requires_session(&self) -> bool {
        self.inner.requires_session()
    }

    /// An unreadable library gives `None`: its users get no image, and [`Kept::kept`] says
    /// why.
    fn code(&self) -> Option<ContentId> {
        self.dylib.code().ok()
    }

    fn expand(&self, db: &Db, call: ProcMacroCall<'_>) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        if compare::comparing() {
            return self.failed(self.inner.expand(db, call));
        }
        if compare::enabled() {
            let expanded = self.inner.expand(db, call);
            compare::again(&self.dylib.path, &self.name, self.kind, db, call, &expanded);
            return self.failed(expanded);
        }
        self.kept(db, call)
    }

    fn expand_in(
        &self,
        db: &Db,
        calling_crate: base_db::Crate,
        call: ProcMacroCall<'_>,
    ) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        if !self.requires_session() {
            return self.expand(db, call);
        }
        // A module that keeps state between calls keeps its sessions itself
        // (blaze-macro-wasm's `Verdict`): its output is a function of the calls before it.
        let start = Instant::now();
        let expanded = self.inner.expand_in(db, calling_crate, call);
        count(Counted::Expanded, start.elapsed());
        count(Counted::of(self.kind), start.elapsed());
        if expanded.is_err() {
            count(Counted::Failed, Duration::ZERO);
        }
        self.failed(expanded)
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool { // dyn: the database's expander trait, the edge
        (other as &dyn Any) // dyn: as above, told apart by type
            .downcast_ref::<Kept>()
            .is_some_and(|other| self.name == other.name && self.inner.eq_dyn(&*other.inner))
    }
}

/// What is counted of expansions, each with the time it took on the thread that did it.
#[derive(Clone, Copy)]
enum Counted {
    /// Calls given a key and looked for.
    Keyed,
    /// Expanded by the macro's server or module, kept or not.
    Expanded,
    /// The same, by kind.
    Derives,
    Attributes,
    FunctionLike,
    /// Read from the store instead.
    Found,
    /// Expanded again under `check`, and the same as kept.
    Checked,
    /// Left for the store's owner to keep ([`puts`]).
    Kept,
    Asked,
    Unspanned,
    Untraced,
    Untraceable,
    Failed,
}

const COUNTED: usize = Counted::Failed as usize + 1;

impl Counted {
    fn of(kind: ProcMacroKind) -> Counted {
        match kind {
            ProcMacroKind::CustomDerive => Counted::Derives,
            ProcMacroKind::Attr => Counted::Attributes,
            ProcMacroKind::Bang => Counted::FunctionLike,
        }
    }
}

static TALLY: tally::Tally<COUNTED> = tally::Tally::new();

thread_local! {
    static MINE: tally::Mine<COUNTED> = tally::Mine::of(&TALLY);
}

fn count(what: Counted, took: Duration) {
    // A thread that is ending has no block left to count into.
    _ = MINE.try_with(|it| it.add(what as usize, took));
}

fn counted(what: Counted) -> (u64, f64) {
    TALLY.get(what as usize)
}

/// The expansions so far, for the daemon's log: `None` before the first.
pub fn report() -> Option<String> {
    let (expanded, expanding) = counted(Counted::Expanded);
    let (found, finding) = counted(Counted::Found);
    if expanded + found == 0 {
        return None;
    }
    let (_, keying) = counted(Counted::Keyed);
    // alloc: the edge, a line of the daemon's log, said once as a build ends
    let mut said = format!(
        "proc macros: {expanded} expanded in {expanding:.2}s, {found} from the store in \
         {finding:.2}s, {keying:.2}s to key and look for them"
    );
    let kinds = [
        ("derives", Counted::Derives),
        ("attributes", Counted::Attributes),
        ("function-like", Counted::FunctionLike),
    ];
    let kinds: Vec<_> = kinds
        .into_iter()
        .filter_map(|(what, kind)| {
            let (calls, took) = counted(kind);
            (calls != 0).then(|| format!("{calls} {what} in {took:.2}s")) // alloc: the edge, as above
        })
        .collect(); // alloc: the edge, as above
    if !kinds.is_empty() {
        said += &format!(" ({})", kinds.join(", ")); // alloc: the edge, as above
    }
    let rest = [
        ("kept", Counted::Kept),
        ("checked against what was kept", Counted::Checked),
        ("not kept, as they asked the database", Counted::Asked),
        ("not kept, as their output has spans the call has not", Counted::Unspanned),
        ("not kept, as their server traces nothing (BLAZE_SANDBOX=off)", Counted::Untraced),
        ("not kept, as they read the clock or randomness", Counted::Untraceable),
        ("failed", Counted::Failed),
    ];
    for (what, which) in rest {
        let (calls, _) = counted(which);
        if calls != 0 {
            said += &format!("; {calls} {what}"); // alloc: the edge, as above
        }
    }
    if let Some(compared) = compare::report() {
        said += &format!("; {compared}"); // alloc: the edge, as above
    }
    Some(said)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod budget {
    use blaze_cas::{Config, Store};
    use blaze_core::{
        FileName, run_until_idle,
        sim::{Faults, Sim},
    };

    use super::*;

    /// A call whose expansion is kept is found through [`Seen`], each variable its shape names
    /// looked up where it is and each file stamped, with no allocation: the hit is read in
    /// place.
    #[cfg(unix)]
    #[test]
    fn a_kept_expansion_is_found_with_no_allocation() {
        let mut sim = Sim::new(5, Faults::default());
        let mut effects = Vec::new();
        let config = Config { dir: FileName::new("store").unwrap(), layers: Vec::new(), tags: 1 << 48 };
        let mut store = Store::open(config, &mut effects);
        run_until_idle(&mut sim, &mut effects, |completion, effects| {
            store.complete(completion, effects);
        });
        assert!(store.ready());
        let lib = std::env::temp_dir().join(format!("blaze-expand-budget-{}", std::process::id()));
        std::fs::write(&lib, b"a macro's library").unwrap();
        let hash = inputs::hash_reader(b"a macro's library".as_slice()).unwrap();
        let run = RunKey { code: ContentId::of("blaze v2 test 1", b"macro"), given: ContentId::default() };
        let traced = Traced {
            files: vec![(lib.clone(), Some(hash))],
            env: vec![
                ("CARGO_PKG_NAME".into(), Some("a".into())),
                ("PATH".into(), Some("/bin".into())),
                ("UNSET".into(), None),
            ],
            ..Traced::default()
        };
        index::keep(&mut store, &run, &traced, b"expansion".to_vec()).unwrap();
        let passed = [("PATH".to_owned(), "/bin".to_owned())];
        let call: Env = [("CARGO_PKG_NAME".to_owned(), "a".to_owned())].into_iter().collect();
        let mut seen = Seen { passed: &passed, call: &call, made: None, none: inputs::Env::default() };
        let reader = store.reader();
        let guard = blaze_cas::enter();
        let values = index::VALUES.traced(&traced);
        let mut find = || {
            matches!(
                index::find_reads(&reader, &guard, &run, &mut seen),
                Ok(Found::Hit { value: b"expansion", values: Some(found), .. }) if found == values
            )
        };
        assert!(find());
        assert!(blaze_alloc_count::assert_at_most(0, "a kept expansion found through Seen", &mut find));
        drop(guard);
        _ = std::fs::remove_file(&lib);
    }

    /// What an expansion that read a file reports is what [`reads_in`] works out of its shape
    /// while the file is as it was, found again on a hit, and not once the file has changed.
    #[cfg(unix)]
    #[test]
    fn the_reads_an_expansion_reports_are_worked_out_again_until_a_file_changes() {
        let mut sim = Sim::new(5, Faults::default());
        let mut effects = Vec::new();
        let config = Config { dir: FileName::new("store").unwrap(), layers: Vec::new(), tags: 1 << 48 };
        let mut store = Store::open(config, &mut effects);
        run_until_idle(&mut sim, &mut effects, |completion, effects| {
            store.complete(completion, effects);
        });
        assert!(store.ready());
        let read = std::env::temp_dir().join(format!("blaze-expand-reads-{}", std::process::id()));
        std::fs::write(&read, b"included").unwrap();
        let hash = inputs::hash_reader(b"included".as_slice()).unwrap();
        let traced = Traced {
            files: vec![(read.clone(), Some(hash))],
            env: vec![("CARGO_PKG_NAME".into(), Some("a".into())), ("UNSET".into(), None)],
            ..Traced::default()
        };
        let reported = index::VALUES.traced(&traced);
        let shape = index::Shape::of(&traced).encode();
        let call: Env = [("CARGO_PKG_NAME".to_owned(), "a".to_owned())].into_iter().collect();
        assert_eq!(reads_in(&[], &call, &shape), Ok(reported));

        let run = RunKey { code: ContentId::of("blaze v2 test 1", b"reads"), given: ContentId::default() };
        index::keep(&mut store, &run, &traced, b"expansion".to_vec()).unwrap();
        let reader = store.reader();
        let guard = blaze_cas::enter();
        let mut seen = Seen { passed: &[], call: &call, made: None, none: inputs::Env::default() };
        match index::find_reads(&reader, &guard, &run, &mut seen) {
            Ok(Found::Hit { shape: found, values, .. }) => {
                assert_eq!(found, shape.as_slice());
                assert_eq!(values, Some(reported));
            }
            other => panic!("{other:?}"),
        }
        drop(guard);

        let other: Env = [("CARGO_PKG_NAME".to_owned(), "b".to_owned())].into_iter().collect();
        assert_ne!(reads_in(&[], &other, &shape), Ok(reported));
        std::fs::write(&read, b"included, and changed").unwrap();
        assert_ne!(reads_in(&[], &call, &shape), Ok(reported));
        _ = std::fs::remove_file(&read);
    }
}
