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

mod compare;
pub mod native;
pub mod puts;
pub mod tally;

use std::{
    any::Any,
    sync::{Arc, Mutex, OnceLock},
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
use hir_expand::proc_macro::{ProcMacroExpander, ProcMacroExpansionError, ProcMacroKind};
use paths::{AbsPath, AbsPathBuf};
use proc_macro_api::{
    legacy_protocol::msg::{FlatTree, SpanDataIndexMap},
    version::CURRENT_API_VERSION,
    warm::{self, Environment},
};
use rustc_hash::FxHashMap;
use span::{Span, SpanAnchor, SyntaxContext, TextRange};

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

/// What the store has of a call.
enum Lookup {
    /// Its expansion, read back.
    Read(tt::TopSubtree),
    /// The hash of its expansion's bytes, to compare a fresh one with ([`Mode::Check`]).
    Hashed(ContentId),
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
    let installed = Installed { expansions, mode };
    // alloc: error path
    INSTALLED.set(installed).map_err(|_| "proc-macro expansions were installed twice".to_owned())?;
    load_cargo::wrap_expanders(wrap);
    Ok(())
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

/// A span as a key or a value has it: its byte range, and the numbers of its anchor and its
/// context among the call's.
pub type Shape = [u32; 4];

/// The anchors and syntax contexts of a call's spans, in the order they first appear: what
/// its output is written against.
#[derive(Default, Debug)]
pub struct Spans {
    anchors: Vec<SpanAnchor>,
    contexts: Vec<SyntaxContext>,
}

impl Spans {
    fn number<T: PartialEq>(all: &mut Vec<T>, it: T) -> u32 {
        let at = all.iter().position(|known| *known == it).unwrap_or_else(|| {
            all.push(it);
            all.len() - 1
        });
        at as u32
    }

    fn shape_new(&mut self, span: &Span) -> Shape {
        let anchor = Self::number(&mut self.anchors, span.anchor);
        let context = Self::number(&mut self.contexts, span.ctx);
        [span.range.start().into(), span.range.end().into(), anchor, context]
    }

    /// The shape of a span of the output, which has to have the call's anchors and contexts.
    fn shape(&self, span: &Span) -> Option<Shape> {
        let anchor = self.anchors.iter().position(|it| *it == span.anchor)?;
        let context = self.contexts.iter().position(|it| *it == span.ctx)?;
        Some([span.range.start().into(), span.range.end().into(), anchor as u32, context as u32])
    }

    fn span(&self, [start, end, anchor, context]: Shape) -> Option<Span> {
        Some(Span {
            range: TextRange::new(start.into(), end.max(start).into()),
            anchor: *self.anchors.get(anchor as usize)?,
            ctx: *self.contexts.get(context as usize)?,
        })
    }
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

fn encoded(value: &impl serde::Serialize) -> Vec<u8> {
    postcard::to_allocvec(value).expect("a flat tree and a list of numbers encode")
}

thread_local! {
    /// The buffer a call's parts are encoded into before [`Given::part`] takes them: one per
    /// thread, grown to the largest call, so a key allocates no encoding.
    static ENCODING: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}

fn part_encoded(given: &mut Given, name: &'static str, value: &impl serde::Serialize) {
    ENCODING.with_borrow_mut(|buffer| {
        buffer.clear();
        let mut written = postcard::to_extend(value, std::mem::take(buffer))
            .expect("a flat tree and a list of numbers encode");
        given.part(name, &written);
        written.clear();
        *buffer = written;
    });
}

/// What `call` gives the macro, for its [`RunKey::given`] with the toolchain `toolchain`, and
/// the anchors and contexts its output is written against. A wasm macro's caller adds what
/// else it gives a module.
pub fn given(toolchain: &ContentId, call: &Call<'_>) -> (Given, Spans) {
    let mut table = SpanDataIndexMap::default();
    for span in [call.def_site, call.call_site, call.mixed_site] {
        table.insert(span);
    }
    let input = FlatTree::from_subtree(call.subtree.view(), CURRENT_API_VERSION, &mut table);
    let attrs =
        call.attrs.map(|it| FlatTree::from_subtree(it.view(), CURRENT_API_VERSION, &mut table));
    let mut spans = Spans::default();
    let shapes: Vec<Shape> = table.iter().map(|span| spans.shape_new(span)).collect();
    let mut given = Given::new(RunKind::ProcMacro);
    given
        .part("name", call.name.as_bytes())
        .part("kind", &[kind_byte(call.kind)])
        .part("abi", &CURRENT_API_VERSION.to_le_bytes())
        .id("toolchain", toolchain)
        .part("dir", call.current_dir.as_bytes());
    part_encoded(&mut given, "input", &input);
    part_encoded(&mut given, "attrs", &attrs);
    part_encoded(&mut given, "spans", &shapes);
    given.env_keyed(call.env.iter().map(|(name, value)| (name.as_bytes(), value.as_bytes())));
    (given, spans)
}

/// The output with its spans as shapes of the call's, or `None` when it has an anchor or a
/// context the call has not.
pub fn written(output: &tt::TopSubtree, spans: &Spans) -> Option<Vec<u8>> {
    let mut table = SpanDataIndexMap::default();
    let flat = FlatTree::from_subtree(output.view(), CURRENT_API_VERSION, &mut table);
    let shapes: Option<Vec<Shape>> = table.iter().map(|span| spans.shape(span)).collect();
    Some(encoded(&(shapes?, flat)))
}

/// What [`written`] wrote, against the call's `spans`.
pub fn read(bytes: &[u8], spans: &Spans) -> Result<tt::TopSubtree, String> {
    let (shapes, flat): (Vec<Shape>, FlatTree) =
        postcard::from_bytes(bytes).map_err(|error| format!("not a kept expansion: {error}"))?;
    let table: Option<SpanDataIndexMap> = shapes.into_iter().map(|it| spans.span(it)).collect();
    let table = table.ok_or("a kept expansion has a span of an anchor or context its call has not")?;
    Ok(flat.to_subtree_resolved(CURRENT_API_VERSION, &table))
}

/// The first expansion that failed for a reason that is not the macro's own (its server died,
/// the store is broken): what is said of it.
static SERVER_FAILED: OnceLock<String> = OnceLock::new();

fn system(said: String) -> ProcMacroExpansionError {
    if SERVER_FAILED.set(said.clone()).is_ok() {
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
        match expanded {
            Err(ProcMacroExpansionError::System(why)) => Err(system(format!(
                "the proc-macro server failed to expand `{}` of {}: {why}; what it did not \
                 expand is missing from the program, so nothing is built from it",
                self.name, self.dylib.path
            ))),
            other => other,
        }
    }

    fn error(&self, why: impl std::fmt::Display) -> ProcMacroExpansionError {
        system(format!("{}: {why}", self.what()))
    }

    /// Expands by the macro's server or module, through the store.
    #[allow(clippy::too_many_arguments)]
    fn kept(
        &self,
        db: &Db,
        subtree: &tt::TopSubtree,
        attrs: Option<&tt::TopSubtree>,
        env: &Env,
        def_site: Span,
        call_site: Span,
        mixed_site: Span,
        current_dir: String,
    ) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        let installed = installed();
        let start = Instant::now();
        let call = Call {
            name: &self.name,
            kind: self.kind,
            subtree,
            attrs,
            env,
            def_site,
            call_site,
            mixed_site,
            current_dir: &current_dir,
        };
        let (given, spans) = given(&installed.expansions.toolchain, &call);
        let code = self.dylib.code().map_err(|why| self.error(why))?;
        let run = RunKey { code, given: given.finish() };
        // The files' table and this call's environment, made only if a shape read a variable:
        // no lock, and nothing here asks the database, so the store's guard is the one thing
        // held across `find`.
        let mut now = Seen {
            passed: installed.expansions.environment.passed(),
            call: env,
            made: None,
            none: inputs::Env::default(),
        };
        let found = {
            let guard = blaze_cas::enter();
            match index::find(&installed.expansions.store, &guard, &run, &mut now) {
                // Read in place, under the guard: a hit copies nothing out of the store.
                Ok(Found::Hit { value, .. }) if installed.mode == Mode::Keep => {
                    read(value, &spans).map(Lookup::Read).map_err(|why| self.error(why))
                }
                Ok(Found::Hit { value, .. }) => Ok(Lookup::Hashed(checked(value))),
                Ok(Found::Miss { .. }) => Ok(Lookup::Nothing),
                Err(FindError::Corrupt(corrupt)) => Err(self.error(corrupt)),
                Err(FindError::NotAShape { key, why }) => {
                    Err(self.error(format!("the store's {key:?} is not a shape: {why}")))
                }
                Err(FindError::Inputs(unreadable)) => Err(self.error(unreadable)),
            }
        }?;
        count(Counted::Keyed, start.elapsed());
        let found = match found {
            Lookup::Read(output) => {
                count(Counted::Found, start.elapsed());
                return Ok(output);
            }
            Lookup::Hashed(hash) => Some(hash),
            Lookup::Nothing => None,
        };
        let asked = load_cargo::asked();
        let expand = Instant::now();
        let expanded =
            self.inner.expand(db, subtree, attrs, env, def_site, call_site, mixed_site, current_dir);
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
        if let Some(kept) = &found {
            if bytes.as_deref().map(checked).as_ref() != Some(kept) {
                return Err(self.error(format!(
                    "{CACHE}=check: it expanded otherwise than the store kept it, under a key \
                     of every input it was traced to read"
                )));
            }
            count(Counted::Checked, Duration::ZERO);
            return Ok(output);
        }
        let traced = match self.keepable(traced, asked) {
            Ok(Some(traced)) => traced,
            Ok(None) => return Ok(output),
            Err(error) => return Err(error),
        };
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
        format!("the expansion of `{}` of {}", self.name, self.dylib.path)
    }

    /// What the expansion read, when it can be kept by it: `None` when it can't be, counted
    /// by why.
    fn keepable(
        &self,
        traced: Option<Result<Traced, String>>,
        asked: u64,
    ) -> Result<Option<Traced>, ProcMacroExpansionError> {
        let traced = match traced {
            Some(Ok(traced)) => traced,
            Some(Err(why)) => return Err(self.error(why)),
            None => {
                count(Counted::Untraced, Duration::ZERO);
                return Ok(None);
            }
        };
        if load_cargo::asked() != asked {
            count(Counted::Asked, Duration::ZERO);
            return Ok(None);
        }
        if index::untraceable(&traced) {
            count(Counted::Untraceable, Duration::ZERO);
            return Ok(None);
        }
        Ok(Some(traced))
    }
}

impl ProcMacroExpander for Kept {
    fn requires_session(&self) -> bool {
        self.inner.requires_session()
    }

    fn expand(
        &self,
        db: &Db,
        subtree: &tt::TopSubtree,
        attrs: Option<&tt::TopSubtree>,
        env: &Env,
        def_site: Span,
        call_site: Span,
        mixed_site: Span,
        current_dir: String,
    ) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        if compare::comparing() {
            let expanded = self.inner.expand(
                db,
                subtree,
                attrs,
                env,
                def_site,
                call_site,
                mixed_site,
                current_dir,
            );
            return self.failed(expanded);
        }
        if compare::enabled() {
            let expanded = self.inner.expand(
                db,
                subtree,
                attrs,
                env,
                def_site,
                call_site,
                mixed_site,
                current_dir.clone(),
            );
            compare::again(
                &self.dylib.path,
                &self.name,
                self.kind,
                db,
                subtree,
                attrs,
                env,
                def_site,
                call_site,
                mixed_site,
                &current_dir,
                &expanded,
            );
            return self.failed(expanded);
        }
        self.kept(db, subtree, attrs, env, def_site, call_site, mixed_site, current_dir)
    }

    fn expand_in(
        &self,
        db: &Db,
        calling_crate: base_db::Crate,
        subtree: &tt::TopSubtree,
        attrs: Option<&tt::TopSubtree>,
        env: &Env,
        def_site: Span,
        call_site: Span,
        mixed_site: Span,
        current_dir: String,
    ) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        if !self.requires_session() {
            return self.expand(
                db,
                subtree,
                attrs,
                env,
                def_site,
                call_site,
                mixed_site,
                current_dir,
            );
        }
        // A module that keeps state between calls keeps its sessions itself
        // (blaze-macro-wasm's `Verdict`): its output is a function of the calls before it.
        let start = Instant::now();
        let expanded = self.inner.expand_in(
            db,
            calling_crate,
            subtree,
            attrs,
            env,
            def_site,
            call_site,
            mixed_site,
            current_dir,
        );
        count(Counted::Expanded, start.elapsed());
        count(Counted::of(self.kind), start.elapsed());
        if expanded.is_err() {
            count(Counted::Failed, Duration::ZERO);
        }
        self.failed(expanded)
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool {
        (other as &dyn Any)
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
            (calls != 0).then(|| format!("{calls} {what} in {took:.2}s"))
        })
        .collect();
    if !kinds.is_empty() {
        said += &format!(" ({})", kinds.join(", "));
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
            said += &format!("; {calls} {what}");
        }
    }
    if let Some(compared) = compare::report() {
        said += &format!("; {compared}");
    }
    Some(said)
}

#[cfg(test)]
mod tests;
