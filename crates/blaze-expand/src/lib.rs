//! Proc-macro expansions kept in the store (docs/v2/usercode.md section 6.5), so that no
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
//! A macro of a crate blaze has a native expander for ([`native`]) is expanded by it, and not
//! kept: `BLAZE_NATIVE_MACROS=verify` expands it both ways and fails the expansion where they
//! differ; `BLAZE_NATIVE_MACROS=off` uses none.
//!
//! [`compare`] holds the toolchain's server against blaze's, and keeps nothing while it does.

mod compare;
pub mod native;

use std::{
    any::Any,
    sync::{
        Arc, Mutex, OnceLock, RwLock,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime},
};

use base_db::{Env, SourceDatabase};
use blaze_cas::Store;
use blaze_core::Traced;
use blaze_ids::ContentId;
use blaze_usercode::{
    Given, RunKey, RunKind,
    index::{self, FindError, Found, KeepError},
    inputs::Disk,
};
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

/// The variable that says how native expanders are used.
pub const NATIVE: &str = "BLAZE_NATIVE_MACROS";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    /// What is found is used.
    Keep,
    /// What is found is expanded again and compared.
    Check,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Native {
    On,
    Verify,
    Off,
}

impl Mode {
    /// The mode `value` of [`CACHE`] says.
    pub fn of(value: Option<&str>) -> Result<Mode, String> {
        match value {
            None => Ok(Mode::Keep),
            Some("check") => Ok(Mode::Check),
            Some(other) => Err(format!(
                "{CACHE}={other:?} is not a mode: every expansion that can be kept is, and \
                 `check` expands what was kept again and compares"
            )),
        }
    }
}

impl Native {
    /// The use `value` of [`NATIVE`] says.
    pub fn of(value: Option<&str>) -> Result<Native, String> {
        match value {
            None => Ok(Native::On),
            Some("verify") => Ok(Native::Verify),
            Some("off") => Ok(Native::Off),
            Some(other) => {
                Err(format!("{NATIVE}={other:?} is not `verify` or `off` (or unset, to use them)"))
            }
        }
    }
}

/// Where expansions are kept, and what they are keyed by besides their call.
pub struct Expansions {
    pub store: Arc<RwLock<Store>>,
    /// The toolchain whose proc-macro ABI and library the macros are built with.
    pub toolchain: ContentId,
    /// What warm servers pass through to the expansions, which they are checked against.
    pub environment: Arc<Environment>,
}

struct Installed {
    expansions: Expansions,
    mode: Mode,
    native: Native,
    /// The machine's files, hashed once per change of what `stat` says of them.
    disk: Mutex<Disk>,
}

static INSTALLED: OnceLock<Installed> = OnceLock::new();

/// Keeps every proc-macro expansion of the workspaces loaded from here on in `expansions`.
/// Says why not when [`CACHE`] or [`NATIVE`] has a value that is not one, or it was installed
/// before.
pub fn install(expansions: Expansions) -> Result<(), String> {
    let var = |name: &str| match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(format!("{name} is not UTF-8")),
    };
    let mode = Mode::of(var(CACHE)?.as_deref())?;
    let native = Native::of(var(NATIVE)?.as_deref())?;
    let installed = Installed { expansions, mode, native, disk: Mutex::new(Disk::new(Vec::new())) };
    INSTALLED.set(installed).map_err(|_| "proc-macro expansions were installed twice".to_owned())?;
    load_cargo::wrap_expanders(wrap);
    Ok(())
}

fn installed() -> &'static Installed {
    INSTALLED.get().expect("an expander is wrapped only once install has been")
}

fn wrap(
    inner: Arc<dyn ProcMacroExpander>,
    dylib: &AbsPath,
    name: &str,
    kind: ProcMacroKind,
) -> Arc<dyn ProcMacroExpander> {
    let native = match installed().native {
        Native::Off => Ok(None),
        Native::On | Native::Verify => native::find(dylib, name, kind),
    };
    Arc::new(Kept { inner, dylib: Dylib::at(dylib), name: name.into(), kind, native })
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
            .or_insert_with(|| Arc::new(Dylib { path: path.to_path_buf(), code: OnceLock::new() }))
            .clone()
    }

    fn code(&self) -> Result<ContentId, String> {
        let code = || match std::fs::read(&self.path) {
            Ok(bytes) => Ok(ContentId::of(LIBRARY, &bytes)),
            Err(error) => Err(format!("the proc-macro library {} can't be read: {error}", self.path)),
        };
        self.code.get_or_init(code).clone()
    }
}

#[derive(Debug)]
struct Kept {
    inner: Arc<dyn ProcMacroExpander>,
    dylib: Arc<Dylib>,
    name: Box<str>,
    kind: ProcMacroKind,
    /// Its native expander, when blaze has one for it, or why finding one failed.
    native: Result<Option<native::Macro>, String>,
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
        .part("input", &encoded(&input))
        .part("attrs", &encoded(&attrs))
        .part("spans", &encoded(&shapes))
        .part("dir", call.current_dir.as_bytes());
    for (name, value) in call.env.iter() {
        given.env(name, value);
    }
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
        system(format!("the expansion of `{}` of {}: {why}", self.name, self.dylib.path))
    }

    /// Expands by the macro's server or module, through the store.
    #[allow(clippy::too_many_arguments)]
    fn kept(
        &self,
        db: &dyn SourceDatabase,
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
        let call_env: Vec<(String, String)> =
            env.iter().map(|(name, value)| (name.to_owned(), value.to_owned())).collect();
        let seen = Environment::seen(&installed.expansions.environment.passed(), &call_env);
        let found = {
            let store = installed.expansions.store.read().unwrap_or_else(|it| it.into_inner());
            let mut disk = installed.disk.lock().unwrap_or_else(|it| it.into_inner());
            disk.set_env(seen);
            match index::find(&store, &run, &mut *disk) {
                Ok(Found::Hit { value, .. }) => Some(value.to_vec()),
                Ok(Found::Miss { .. }) => None,
                Err(FindError::Corrupt(corrupt)) => return Err(self.error(corrupt)),
                Err(FindError::NotAShape { key, why }) => {
                    return Err(self.error(format!("the store's {key:?} is not a shape: {why}")));
                }
                Err(FindError::Inputs(unreadable)) => return Err(self.error(unreadable)),
            }
        };
        COUNTS.keyed.add(start.elapsed());
        if let Some(bytes) = &found
            && installed.mode == Mode::Keep
        {
            let output = read(bytes, &spans).map_err(|why| self.error(why))?;
            COUNTS.found.add(start.elapsed());
            return Ok(output);
        }
        let asked = load_cargo::asked();
        let expand = Instant::now();
        let expanded =
            self.inner.expand(db, subtree, attrs, env, def_site, call_site, mixed_site, current_dir);
        let traced = warm::take_traced();
        COUNTS.expanded.add(expand.elapsed());
        COUNTS.kinds[kind_byte(self.kind) as usize].add(expand.elapsed());
        let output = match self.failed(expanded) {
            Ok(output) => output,
            Err(error) => {
                COUNTS.failed.add(Duration::ZERO);
                return Err(error);
            }
        };
        let bytes = written(&output, &spans);
        if let Some(kept) = &found {
            if bytes.as_ref() != Some(kept) {
                return Err(self.error(format!(
                    "{CACHE}=check: it expanded otherwise than the store kept it, under a key \
                     of every input it was traced to read"
                )));
            }
            COUNTS.checked.add(Duration::ZERO);
            return Ok(output);
        }
        let traced = match self.keepable(traced, asked) {
            Ok(Some(traced)) => traced,
            Ok(None) => return Ok(output),
            Err(error) => return Err(error),
        };
        let Some(bytes) = bytes else {
            COUNTS.unspanned.add(Duration::ZERO);
            return Ok(output);
        };
        let mut store = installed.expansions.store.write().unwrap_or_else(|it| it.into_inner());
        match index::keep(&mut store, &run, &traced, bytes) {
            Ok(_) => COUNTS.kept.add(Duration::ZERO),
            Err(KeepError::Corrupt(corrupt)) => return Err(self.error(corrupt)),
            Err(KeepError::Mismatch(mismatch)) => {
                return Err(self.error(format!(
                    "the store has another expansion under the same key of every input it read, \
                     so the key misses an input or the macro is not deterministic: {mismatch:?}"
                )));
            }
        }
        Ok(output)
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
                COUNTS.untraced.add(Duration::ZERO);
                return Ok(None);
            }
        };
        if load_cargo::asked() != asked {
            COUNTS.asked.add(Duration::ZERO);
            return Ok(None);
        }
        if index::untraceable(&traced) {
            COUNTS.untraceable.add(Duration::ZERO);
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
        db: &dyn SourceDatabase,
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
        let native = match &self.native {
            Ok(native) => native.as_ref(),
            Err(why) => return Err(self.error(format!("its native expander: {why}"))),
        };
        if let Some(native) = native {
            let start = Instant::now();
            let natively =
                native.expand(db, subtree, attrs, env, def_site, call_site, mixed_site);
            COUNTS.native.add(start.elapsed());
            if installed().native != Native::Verify {
                return natively;
            }
            let normally = self.kept(
                db,
                subtree,
                attrs,
                env,
                def_site,
                call_site,
                mixed_site,
                current_dir,
            )?;
            let natively = natively.map_err(|error| {
                self.error(format!("{NATIVE}=verify: its native expander failed: {error:?}"))
            })?;
            if let Some(difference) = native::difference(&natively, &normally) {
                return Err(self.error(format!(
                    "{NATIVE}=verify: its native expander's output differs from the macro's: \
                     {difference}"
                )));
            }
            COUNTS.verified.add(Duration::ZERO);
            return Ok(normally);
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
        db: &dyn SourceDatabase,
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
        COUNTS.expanded.add(start.elapsed());
        COUNTS.kinds[kind_byte(self.kind) as usize].add(start.elapsed());
        if expanded.is_err() {
            COUNTS.failed.add(Duration::ZERO);
        }
        self.failed(expanded)
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool {
        (other as &dyn Any)
            .downcast_ref::<Kept>()
            .is_some_and(|other| self.name == other.name && self.inner.eq_dyn(&*other.inner))
    }
}

/// How many, and the time they took on the threads that expanded them.
struct Count {
    calls: AtomicU64,
    nanos: AtomicU64,
}

impl Count {
    const fn new() -> Count {
        Count { calls: AtomicU64::new(0), nanos: AtomicU64::new(0) }
    }

    fn add(&self, took: Duration) {
        self.calls.fetch_add(1, Ordering::Relaxed);
        self.nanos.fetch_add(took.as_nanos() as u64, Ordering::Relaxed);
    }

    fn get(&self) -> (u64, f64) {
        (self.calls.load(Ordering::Relaxed), self.nanos.load(Ordering::Relaxed) as f64 / 1e9)
    }
}

struct Counts {
    /// Calls given a key and looked for, with the time that took.
    keyed: Count,
    /// Expanded by the macro's server or module, kept or not.
    expanded: Count,
    /// The same, by [`kind_byte`].
    kinds: [Count; 3],
    /// Read from the store instead.
    found: Count,
    /// Expanded again under `check`, and the same as kept.
    checked: Count,
    /// Expanded by a native expander.
    native: Count,
    /// Expanded both ways under `verify`, and the same.
    verified: Count,
    kept: Count,
    asked: Count,
    unspanned: Count,
    untraced: Count,
    untraceable: Count,
    failed: Count,
}

static COUNTS: Counts = Counts {
    keyed: Count::new(),
    expanded: Count::new(),
    kinds: [Count::new(), Count::new(), Count::new()],
    found: Count::new(),
    checked: Count::new(),
    native: Count::new(),
    verified: Count::new(),
    kept: Count::new(),
    asked: Count::new(),
    unspanned: Count::new(),
    untraced: Count::new(),
    untraceable: Count::new(),
    failed: Count::new(),
};

/// The expansions so far, for the daemon's log: `None` before the first.
pub fn report() -> Option<String> {
    let (expanded, expanding) = COUNTS.expanded.get();
    let (found, finding) = COUNTS.found.get();
    let (native, natively) = COUNTS.native.get();
    if expanded + found + native == 0 {
        return None;
    }
    let (_, keying) = COUNTS.keyed.get();
    let mut said = format!(
        "proc macros: {expanded} expanded in {expanding:.2}s, {found} from the store in \
         {finding:.2}s, {keying:.2}s to key and look for them, {native} natively in \
         {natively:.2}s"
    );
    let kinds = ["derives", "attributes", "function-like"].iter().zip(&COUNTS.kinds);
    let kinds: Vec<_> = kinds
        .filter_map(|(what, count)| {
            let (calls, took) = count.get();
            (calls != 0).then(|| format!("{calls} {what} in {took:.2}s"))
        })
        .collect();
    if !kinds.is_empty() {
        said += &format!(" ({})", kinds.join(", "));
    }
    let rest = [
        ("kept", &COUNTS.kept),
        ("checked against what was kept", &COUNTS.checked),
        ("verified against their native expander", &COUNTS.verified),
        ("not kept, as they asked the database", &COUNTS.asked),
        ("not kept, as their output has spans the call has not", &COUNTS.unspanned),
        ("not kept, as their server traces nothing (BLAZE_SANDBOX=off)", &COUNTS.untraced),
        ("not kept, as they read the clock or randomness", &COUNTS.untraceable),
        ("failed", &COUNTS.failed),
    ];
    for (what, count) in rest {
        let (calls, _) = count.get();
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
