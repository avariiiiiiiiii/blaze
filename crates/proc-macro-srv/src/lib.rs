//! RA Proc Macro Server
//!
//! This library is able to call compiled Rust custom derive dynamic libraries on arbitrary code.
//! The general idea here is based on <https://github.com/fedochet/rust-proc-macro-expander>.
//!
//! But we adapt it to better fit RA needs:
//!
//! * We use `tt` for proc-macro `TokenStream` server, it is easier to manipulate and interact with
//!   RA than `proc-macro2` token stream.

#![feature(
    allocator_api,
    allocator_ext,
    proc_macro_internals,
    proc_macro_diagnostic,
    proc_macro_span
)]
#![expect(internal_features)]
#![allow(unused_features, unused_crate_dependencies)]
#![deny(deprecated_safe, clippy::undocumented_unsafe_blocks)]
#![cfg_attr(test, expect(unreachable_pub))]

// The standard library's `proc_macro`, which rustc's tree builds as `rustc_proc_macro`.
extern crate proc_macro as rustc_proc_macro;
mod bridge;
mod dylib;
mod scope;
mod server_impl;
mod token_stream;
mod wire;

use std::{
    env,
    ffi::OsString,
    fs,
    ops::Range,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, PoisonError},
    thread,
};

use paths::{Utf8Path, Utf8PathBuf};
use rustc_hash::{FxHashMap, FxHashSet};
use span::{FIXUP_ERASED_FILE_AST_ID_MARKER, Span};

pub use crate::server_impl::token_id::SpanId;

pub use rustc_proc_macro::Delimiter;
pub use span;

pub use crate::bridge::*;
pub use crate::server_impl::literal_from_str;
pub use crate::token_stream::{TokenStream, TokenStreamIter, literal_to_string};

#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum ProcMacroKind {
    CustomDerive,
    Attr,
    Bang,
}

/// The `rustc --version` of the toolchain in `harness/toolchain` (nightly-2026-10-27, rust-lang/rust
/// at 16d1ffac1), whose `proc_macro` this server is built over; upstream's build script asks rustc.
pub const RUSTC_VERSION_STRING: &str = "rustc 1.101.0-nightly (16d1ffac1 2026-10-26)";

pub struct ProcMacroSrv<'env> {
    expanders: Mutex<FxHashMap<Utf8PathBuf, Arc<dylib::Expander>>>,
    env: &'env EnvSnapshot,
    /// How many expansions it was asked for: the next one's scope ([`scope`]).
    expansions: std::sync::atomic::AtomicU64,
}

impl<'env> ProcMacroSrv<'env> {
    pub fn new(env: &'env EnvSnapshot) -> Self {
        Self { expanders: Default::default(), env, expansions: Default::default() }
    }

    /// Counts an expansion asked for whose request could not be read, so [`Self::expand`] is
    /// never called for it: the client counts every expansion it asks for, and the next one's
    /// scope must be the one it expects.
    pub fn count_unexpanded(&self) {
        self.expansions.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    pub fn join_spans(&self, first: Span, second: Span) -> Option<Span> {
        first.join(second, |_, _| {
            // FIXME: Once we can talk back to the client, implement a "long join" request for anchors
            // that differ in [AstId]s as joining those spans requires resolving the AstIds.
            None
        })
    }
}

#[derive(Debug)]
pub enum ProcMacroClientError {
    Cancelled { reason: String },
    Io(std::io::Error),
    Protocol(String),
    Eof,
}

#[derive(Debug)]
pub enum ProcMacroPanicMarker {
    Cancelled { reason: String },
    Internal { reason: String },
}

// The server program's client of blaze, which this crate can't name (the program depends on it).
// dyn: asked once per span question a macro asks, each a round trip to blaze.
pub type ProcMacroClientHandle<'a> = &'a mut (dyn ProcMacroClientInterface + Sync + Send);

pub trait ProcMacroClientInterface {
    fn file(&mut self, file_id: span::FileId) -> String;
    fn source_text(&mut self, span: Span) -> Option<String>;
    fn local_file(&mut self, file_id: span::FileId) -> Option<String>;
    /// Line and column are 1-based.
    fn line_column(&mut self, span: Span) -> Option<(u32, u32)>;

    fn byte_range(&mut self, span: Span) -> Range<usize>;
    fn span_source(&mut self, span: Span) -> Span;
    fn span_parent(&mut self, span: Span) -> Option<Span>;
    fn span_join(&mut self, first: Span, second: Span) -> Option<Span>;
}

const EXPANDER_STACK_SIZE: usize = 8 * 1024 * 1024;

pub enum ExpandError {
    Panic(PanicMessage),
    Cancelled { reason: Option<String> },
    Internal { reason: Option<String> },
}

impl ExpandError {
    pub fn into_string(self) -> Option<String> {
        match self {
            ExpandError::Panic(panic_message) => panic_message.into_string(),
            ExpandError::Cancelled { reason } => reason,
            ExpandError::Internal { reason } => reason,
        }
    }
}

impl ProcMacroSrv<'_> {
    pub fn expand<'a, S: ProcMacroSrvSpan + 'a>(
        &self,
        lib: impl AsRef<Utf8Path>,
        env: &[(String, String)],
        current_dir: Option<impl AsRef<Path>>,
        macro_name: &str,
        macro_body: token_stream::TokenStream<S>,
        attribute: Option<token_stream::TokenStream<S>>,
        def_site: S,
        call_site: S,
        mixed_site: S,
        tracked_env: &'a mut TrackedEnv,
        callback: Option<ProcMacroClientHandle<'a>>,
    ) -> Result<token_stream::TokenStream<S>, ExpandError> {
        // Counted before anything can fail: the client counts every expansion it asks for.
        let number = self.expansions.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let snapped_env = self.env;
        let expander = self.expander(lib.as_ref()).map_err(|err| ExpandError::Internal {
            // alloc: error path, naming why the library didn't load.
            reason: Some(format!("failed to load macro: {err}")),
        })?;

        let prev_env = EnvChange::apply(snapped_env, env, current_dir.as_ref().map(<_>::as_ref));

        // Note, we spawn a new thread here so that thread locals allocation don't accumulate (this
        // includes the proc-macro symbol interner)
        let result = thread::scope(|s| {
            let thread = thread::Builder::new()
                .stack_size(EXPANDER_STACK_SIZE)
                // The thread keeps the macro's thread locals from outliving the expansion.
                // alloc: its name, beside spawning it.
                .name(macro_name.to_owned())
                .spawn_scoped(s, move || {
                    let _marked = scope::mark(number);
                    expander.expand(
                        macro_name,
                        macro_body,
                        attribute,
                        def_site,
                        call_site,
                        mixed_site,
                        tracked_env,
                        callback,
                    )
                });
            match thread.unwrap().join() {
                Ok(res) => res.map_err(ExpandError::Panic),
                Err(payload) => match payload.downcast::<ProcMacroPanicMarker>() {
                    Ok(marker) => match *marker {
                        ProcMacroPanicMarker::Cancelled { reason } => {
                            Err(ExpandError::Cancelled { reason: Some(reason) })
                        }
                        ProcMacroPanicMarker::Internal { reason } => {
                            Err(ExpandError::Internal { reason: Some(reason) })
                        }
                    },
                    Err(payload) => std::panic::resume_unwind(payload),
                },
            }
        });
        prev_env.rollback();

        result
    }

    pub fn list_macros(
        &self,
        dylib_path: &Utf8Path,
    ) -> Result<Vec<(String, ProcMacroKind)>, String> {
        let expander = self.expander(dylib_path)?;
        // alloc: the library's macros, listed once per library for the client, which owns them.
        Ok(expander.list_macros().map(|(k, v)| (k.to_owned(), v)).collect())
    }

    /// The loaded library at `path`, loaded again when it changed since. A library already
    /// loaded is found by `path` as given: only loading one stores a key.
    fn expander(&self, path: &Utf8Path) -> Result<Arc<dylib::Expander>, String> {
        let mut expanders = self.expanders.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(loaded) = expanders.get_mut(path) {
            let time = fs::metadata(path).and_then(|it| it.modified()).ok();
            if Some(loaded.modified_time()) != time {
                *loaded = load(path)?;
            }
            // clone: the expander's `Arc`, a handle for this expansion.
            return Ok(loaded.clone());
        }
        let loaded = load(path)?;
        // alloc: clone: the library's key and its `Arc`'s second handle, once per library loaded.
        expanders.insert(path.to_path_buf(), loaded.clone());
        Ok(loaded)
    }
}

/// The library at `path`, loaded: once per library, or per change to it.
fn load(path: &Utf8Path) -> Result<Arc<dylib::Expander>, String> {
    match dylib::Expander::new(path) {
        // alloc: the loaded library's one shared handle.
        Ok(expander) => Ok(Arc::new(expander)),
        // alloc: error path, naming the library.
        Err(err) => Err(format!("Cannot create expander for {path}: {err}")),
    }
}

#[derive(Default)]
pub struct TrackedEnv {
    pub env_vars: FxHashMap<Box<str>, Option<Box<str>>>,
    pub paths: FxHashSet<Box<str>>,
}

pub trait ProcMacroSrvSpan: Copy + Send + Sync {
    type Server<'a>: rustc_proc_macro::bridge::server::Server<
            TokenStream = crate::token_stream::TokenStream<Self>,
        >;
    fn make_server<'a>(
        call_site: Self,
        def_site: Self,
        mixed_site: Self,
        tracked_env: &'a mut TrackedEnv,
        callback: Option<ProcMacroClientHandle<'a>>,
    ) -> Self::Server<'a>;
}

impl ProcMacroSrvSpan for SpanId {
    type Server<'a> = server_impl::token_id::SpanIdServer<'a>;

    fn make_server<'a>(
        call_site: Self,
        def_site: Self,
        mixed_site: Self,
        _: &'a mut TrackedEnv,
        callback: Option<ProcMacroClientHandle<'a>>,
    ) -> Self::Server<'a> {
        Self::Server { call_site, def_site, mixed_site, callback }
    }
}

impl ProcMacroSrvSpan for Span {
    type Server<'a> = server_impl::rust_analyzer_span::RaSpanServer<'a>;
    fn make_server<'a>(
        call_site: Self,
        def_site: Self,
        mixed_site: Self,
        tracked_env: &'a mut TrackedEnv,
        callback: Option<ProcMacroClientHandle<'a>>,
    ) -> Self::Server<'a> {
        Self::Server {
            call_site,
            def_site,
            mixed_site,
            callback,
            tracked_env,
            fixup_id: FIXUP_ERASED_FILE_AST_ID_MARKER,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PanicMessage {
    message: Option<String>,
}

impl PanicMessage {
    pub fn into_string(self) -> Option<String> {
        self.message
    }
}

/// The value the server started with of each variable an expansion was given, read by its name
/// the first time one sets it. Walking `environ` would read every variable, which a warm
/// server's sandbox traces as each expansion reading all of them.
#[derive(Default)]
pub struct EnvSnapshot {
    /// Locked only while `ENV_LOCK` is held.
    vars: Mutex<FxHashMap<Box<str>, Option<OsString>>>,
}

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

struct EnvChange<'snap> {
    /// The variables set, each put back as the snapshot has it on drop.
    changed_vars: &'snap [(String, String)],
    prev_working_dir: Option<PathBuf>,
    snap: &'snap EnvSnapshot,
    _guard: std::sync::MutexGuard<'snap, ()>,
}

impl<'snap> EnvChange<'snap> {
    fn apply(
        snap: &'snap EnvSnapshot,
        new_vars: &'snap [(String, String)],
        current_dir: Option<&Path>,
    ) -> EnvChange<'snap> {
        let guard = ENV_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let prev_working_dir = match current_dir {
            Some(dir) => {
                let prev_working_dir = std::env::current_dir().ok();
                if let Err(err) = std::env::set_current_dir(dir) {
                    eprintln!(
                        "Failed to change the current working dir to {}. Error: {err:?}",
                        dir.display()
                    )
                }
                prev_working_dir
            }
            None => None,
        };
        let mut started = snap.vars.lock().unwrap_or_else(PoisonError::into_inner);
        for (k, v) in new_vars {
            if !started.contains_key(k.as_str()) {
                // alloc: the variable's name and first value, once per name the server is given.
                started.insert(k.as_str().into(), env::var_os(k));
            }
            // SAFETY: `set_var` needs no other thread reading or writing the environment as it
            // changes. `ENV_LOCK` is held from here until this `EnvChange` drops, after `expand`
            // has joined the macro's thread, so no expansion runs while it changes, and no other
            // thread of the server reads the environment. A thread that did (a macro's own
            // thread left running, a C library's `getenv`) would race with the write.
            unsafe { env::set_var(k, v) };
        }
        drop(started);
        EnvChange { snap, changed_vars: new_vars, prev_working_dir, _guard: guard }
    }

    fn rollback(self) {}
}

impl Drop for EnvChange<'_> {
    fn drop(&mut self) {
        let started = self.snap.vars.lock().unwrap_or_else(PoisonError::into_inner);
        for (name, _) in self.changed_vars {
            match started.get(name.as_str()) {
                // SAFETY: `self._guard` still holds `ENV_LOCK` and the macro's thread has been
                // joined, as in `apply`, so no other thread reads the environment as it changes.
                Some(Some(prev_val)) => unsafe { env::set_var(name, prev_val) },
                // SAFETY: as for `set_var` just above.
                _ => unsafe { env::remove_var(name) },
            }
        }
        drop(started);

        if let Some(dir) = &self.prev_working_dir
            && let Err(err) = std::env::set_current_dir(dir)
        {
            eprintln!(
                "Failed to change the current working dir back to {}. Error: {:?}",
                dir.display(),
                err
            )
        }
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

/// The library of `proc-macro-test-impl` that blaze linked for these tests, by
/// `proc-macro-libraries` in this package's manifest.
#[cfg(test)]
pub fn proc_macro_test_dylib_path() -> paths::Utf8PathBuf {
    env!("BLAZE_PROC_MACRO_LIBRARY_PROC_MACRO_TEST_IMPL").into()
}
