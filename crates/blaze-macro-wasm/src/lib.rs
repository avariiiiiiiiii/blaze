//! Proc macros compiled to WebAssembly and run under Wasmtime in blaze's own process, as
//! dtolnay's watt runs them.
//!
//! blaze compiles a proc-macro crate for `wasm32-wasip1` with [`GUEST`] in place of the
//! standard library's `proc_macro`: the same API, whose token streams live in the module's
//! memory and whose spans are words of the host's span parts, so syn, quote and proc-macro2
//! build against it unchanged. An expansion is given its input as a token tree's columns
//! ([`wire`]), asks the host what only the host knows (what a span joins to, a string lexed),
//! and hands back its output as the columns blaze's builder makes of it, which the host reads
//! as they are.
//!
//! Whether a module keeps state between expansions is seen, not guessed: the guest's
//! allocator counts the bytes live, and an expansion that leaves some behind tells the host
//! ([`Verdict`]). A module none of whose expansions did is stateless: each expansion is a
//! fresh instance ([`expand`]), a run of user code kept by what it was given and what it read
//! ([`blaze_usercode::index`], in blaze-expand's store), and crates that call it are frozen as
//! any other. A module one of whose expansions did, or
//! one not seen yet, keeps an instance per calling crate for one database revision
//! ([`Session`]): its statics, stored tokens and their spans survive its calls, a source
//! change starts a fresh session and reruns demanded expansions, and nothing it makes is kept.
//!
//! What a module can learn of the machine is what it is given: its package's directory to
//! read, at a path that is the same everywhere ([`wasi::PACKAGE`]), and cargo's variables for
//! its package. Every read of the package is recorded, so that what one machine expanded
//! every other finds, whatever it runs on: neither the module, which is the same bytes for
//! every host, nor its expansions are kept under anything of the host's. What an expansion
//! asked the database of a span of its input (its text, file, line or bytes) is kept with it,
//! each question with its answer, and it is found while the database answers each the same.
//! One that was given a span it was not ([`Value`]) is not kept, nor one that read the clock or
//! randomness, which it is given as a native macro is.
//!
//! Which macros run as modules is [`rule`]'s.

pub mod bench;
mod keyed;
pub mod rule;
mod wasi;

use salsa::Database as _;
use std::{
    any::Any,
    borrow::Cow,
    ffi::{OsStr, OsString},
    fmt::{self, Write as _},
    io::Write as _,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, LazyLock, Mutex, OnceLock,
        atomic::Ordering,
    },
    thread::ThreadId,
    time::{Duration, Instant},
};

use base_db::{Db, Crate, Env, Nonce};
use blaze_core::Traced;
use blaze_expand::{
    Mode, Put,
    bridge::{
        lex,
        spans::{self, range, spanned},
        wire::{self, NONE, Part, Parts, Tree},
    },
    tally::{Mine, Tally},
    verdict::{STATEFUL, STATELESS, Seen, Verdict},
};
use blaze_ids::{ContentId, Hasher, Idx, Interner, Sharded};
use blaze_usercode::{
    Given, RunKey, RunKind,
    index::{self, FindError, Found},
    inputs::{self, Files, Unreadable},
    trace::{Inputs, Names},
};
use hir_expand::proc_macro::{
    ProcMacro, ProcMacroCall, ProcMacroExpander, ProcMacroExpansionError, ProcMacroKind,
    ProcMacroLoadResult,
};
use intern::Symbol;
use load_cargo::{SubRequest, SubResponse};
use paths::AbsPath;
use rustc_hash::FxHashMap;
use smallvec::SmallVec;
use span::{FIXUP_ERASED_FILE_AST_ID_MARKER, Span, SpanAnchor, SyntaxContext, TextRange};
use wasmtime::{
    Caller, Config, Enabled, Engine, Extern, Instance, InstanceAllocationStrategy, InstancePre,
    Linker, Memory, Module, ModuleExport, PoolingAllocationConfig, Store, StoreLimits,
    StoreLimitsBuilder, TypedFunc,
};

#[cfg(test)]
#[global_allocator]
static ALLOC: blaze_alloc_count::Counting = blaze_alloc_count::Counting;

/// The `proc_macro` crate a proc macro compiled to WebAssembly is built against: the guest's
/// imports and exports, then the API the in-process bridge's `proc_macro` includes too.
pub const GUEST: &str = concat!(
    include_str!("../guest/proc_macro.rs"),
    include_str!("../../blaze-expand/src/native/proc_macro/api.rs"),
);

/// The functions a module of proc macros exports, which blaze puts in the crate
/// (`crates/blaze-engine/src/macro_list.rs`).
pub mod exports {
    /// `(len) -> ptr`: room for an input of `len` bytes.
    pub const ALLOC: &str = "blaze_proc_macro_alloc";
    /// `()`: the list of the crate's macros, as output, a line each: kind (0 derive, 1
    /// attribute, 2 function-like), name and a derive's helper attributes, tab-separated.
    pub const LIST: &str = "blaze_proc_macro_list";
    /// `(index, attr_ptr, attr_len, ptr, len)`: expands the macro at `index` of the list.
    pub const EXPAND: &str = "blaze_proc_macro_expand";
    /// The custom section that holds what [`LIST`] gives, for the host to read without
    /// compiling the module.
    pub const LIST_SECTION: &str = "blaze_proc_macros";
}

/// The host's functions the module imports.
const HOST: &str = "blaze_proc_macro";

/// The version of what this host gives a module (its functions, its WASI, how it writes
/// tokens), in what each expansion is given: another when any of that changes.
const HOST_VERSION: &str = "blaze v2 wasm proc-macro host 2";

/// The most memory one instance may have.
const MEMORY: usize = 4 << 30;

/// Where a module's instances come from. A fresh instance (an expansion of a module that keeps
/// no state, or a run of its list) lives for one call, so it takes a slot of a pool: the slot's
/// memory stays mapped between instances, and on Linux its pages stay resident and are reset
/// in place, so an instance costs no `mmap`, no `munmap` and no page fault on what the last
/// one touched. A session lives as long as its crate's revision, and a build can hold more of
/// them than any pool has slots, so its instance is made on demand.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Fresh = 0,
    Session = 1,
}

impl Kind {
    /// The kind the instances of a module of `verdict` are.
    fn of(verdict: Verdict) -> Kind {
        match verdict {
            Verdict::Stateless => Kind::Fresh,
            Verdict::Unseen | Verdict::Stateful => Kind::Session,
        }
    }
}

/// The engine sessions and tests use.
fn engine() -> &'static Engine {
    engine_of(Kind::Session)
}

/// The engine of `kind`'s instances. The two differ only in how they allocate instances, which
/// is not in what machine code must match to load (`Engine::precompile_compatibility_hash`), so
/// a module's code in the store serves both.
fn engine_of(kind: Kind) -> &'static Engine {
    static ENGINES: [OnceLock<Engine>; 2] = [OnceLock::new(), OnceLock::new()];
    ENGINES[kind as usize].get_or_init(|| {
        let mut config = Config::new();
        config.cranelift_nan_canonicalization(true);
        config.parallel_compilation(true);
        // A target given turns off Cranelift's reading of the host's CPU features, which
        // would put them in `code_key` and part hosts of one kind (CI's runners are AMD and
        // Intel) that should share machine code.
        // alloc: setup, once a process: the engine's target by name.
        config.target(&target_lexicon::HOST.to_string()).expect("the host is a Wasmtime target");
        for flag in CPU_FEATURES {
            // SAFETY: each is a feature Wasmtime checks the host has before it runs code
            // compiled with it, or (the return-address signing) what the host's ABI needs.
            unsafe { config.cranelift_flag_enable(flag) };
        }
        if kind == Kind::Fresh {
            config.allocation_strategy(InstanceAllocationStrategy::Pooling(pool()));
        }
        Engine::new(&config).expect("Wasmtime's configuration for proc macros")
    })
}

/// How many fresh instances may be live at once: each slot reserves its memory's 4 GiB and
/// guard of address space, untouched until used, so about 4 TiB in all.
const SLOTS: u32 = 1000;

/// The most elements a fresh instance's table may have: a module's table holds each function
/// whose address it takes, a few thousand for one built on syn.
const TABLE_ELEMENTS: usize = 1 << 17;

/// The most an instance's own metadata may take, which grows with its functions and imports.
const INSTANCE_SIZE: usize = 8 << 20;

/// What of a slot's memory stays resident between instances, reset in place: about what one
/// expansion of a macro built on syn touches.
const KEEP_RESIDENT: usize = 1 << 20;

fn pool() -> PoolingAllocationConfig {
    let mut pool = PoolingAllocationConfig::new();
    pool.total_core_instances(SLOTS)
        .total_memories(SLOTS)
        .total_tables(2 * SLOTS)
        .max_memories_per_module(1)
        .max_tables_per_module(2)
        .max_memory_size(MEMORY)
        .table_elements(TABLE_ELEMENTS)
        .max_core_instance_size(INSTANCE_SIZE)
        .linear_memory_keep_resident(KEEP_RESIDENT)
        .table_keep_resident(64 << 10)
        // Linux 6.7's `PAGEMAP_SCAN` finds the pages an instance touched, so only those are
        // reset; elsewhere the resident bytes are reset whole.
        .pagemap_scan(Enabled::Auto);
    pool
}

/// The pool's slots, counted here so that a thread that finds none free waits for one to be
/// given back rather than Wasmtime refusing the instance: whether an expansion runs never
/// depends on how many others run at once. The lock is held only to count.
struct Slots {
    free: Mutex<u32>,
    given: Condvar,
}

static FREE: Slots = Slots { free: Mutex::new(SLOTS), given: Condvar::new() };

/// A slot of the pool, taken for one fresh instance and given back when dropped, which its
/// holder does after the instance's store.
struct Slot(());

impl Slot {
    fn take() -> Slot {
        let mut free = FREE.free.lock().unwrap_or_else(|it| it.into_inner());
        while *free == 0 {
            free = FREE.given.wait(free).unwrap_or_else(|it| it.into_inner());
        }
        *free -= 1;
        Slot(())
    }

    /// A slot when `kind`'s instances take one.
    fn for_kind(kind: Kind) -> Option<Slot> {
        (kind == Kind::Fresh).then(Slot::take)
    }
}

impl Drop for Slot {
    fn drop(&mut self) {
        *FREE.free.lock().unwrap_or_else(|it| it.into_inner()) += 1;
        FREE.given.notify_one();
    }
}

/// The CPU features machine code is compiled for, the same on every host of an arch: on
/// x86-64 its v2 level, which every x86-64 CPU since 2009 has and which keeps the floating
/// point Wasm needs off host calls; on arm64 none beyond the base but the return-address
/// signing macOS requires.
const CPU_FEATURES: &[&str] = if cfg!(target_arch = "x86_64") {
    &["has_cmpxchg16b", "has_sse3", "has_ssse3", "has_sse41", "has_sse42", "has_popcnt"]
} else if cfg!(all(target_arch = "aarch64", target_os = "macos")) {
    &["sign_return_address", "sign_return_address_with_bkey"]
} else {
    &[]
};

/// The database a running expansion asks, for as long as it runs.
#[derive(Clone, Copy)]
struct SessionDb(*const Db);

// SAFETY: a `SessionDb` is only read on the thread that expands, while `Session::run` holds the
// database it points to, and clears the pointer even when the host unwinds.
unsafe impl Send for SessionDb {}
// SAFETY: as for `Send`: the pointer is dereferenced only on the thread that expands, while
// `Session::run` holds the database, which it clears before returning or resuming an unwind.
unsafe impl Sync for SessionDb {}

/// What an instance has while it runs.
pub(crate) struct State {
    parts: Parts,
    /// The words of the def, call and mixed sites.
    sites: [u64; 3],
    files: wasi::Files,
    /// The module's environment, sorted by name ([`guest_env`]).
    environ: Environ,
    /// The instance's memory, found once it is made, before any of its code runs: a host
    /// function reads it with no lookup by name.
    memory: Option<Memory>,
    /// What a function that gives text or a stream back put there, for the module's `take`.
    buffer: Vec<u8>,
    /// What the module gave as its output, once [`State::gave`] says it did.
    output: Vec<u8>,
    gave: bool,
    panic: Option<String>,
    /// Whether the expansion left what it allocated live for a later one.
    retained: bool,
    /// Whether the expansion learned of a span what neither its key nor [`State::asks`] says:
    /// then it is not kept.
    asked: bool,
    /// The anchors and contexts of the input's spans, which a question is written against.
    anchors: Vec<SpanAnchor>,
    contexts: Vec<SyntaxContext>,
    /// Each question the expansion asked of a span of its input and what it was answered, as a
    /// read set has them ([`Value::read`]).
    asks: Questions,
    db: Option<SessionDb>,
    limits: StoreLimits,
}

impl State {
    pub(crate) fn empty() -> State {
        State {
            parts: Parts::default(),
            sites: [NONE; 3],
            files: wasi::Files::new(None),
            environ: Environ::default(),
            memory: None,
            // alloc: none, an empty list until the instance runs.
            buffer: Vec::new(),
            // alloc: none, an empty list until the instance runs.
            output: Vec::new(),
            gave: false,
            panic: None,
            retained: false,
            asked: false,
            // alloc: none, an empty list until the instance runs.
            anchors: Vec::new(),
            // alloc: none, an empty list until the instance runs.
            contexts: Vec::new(),
            asks: Questions::default(),
            db: None,
            limits: StoreLimitsBuilder::new().memory_size(MEMORY).build(),
        }
    }

    fn ask(&mut self, req: SubRequest) -> Option<SubResponse> {
        self.asked = true;
        let db = self.db?;
        // SAFETY: `self.db` is `Some` only inside `Session::run`, which set it from a `&Db`
        // borrowed for that call and clears it before returning or resuming an unwind; host
        // functions, this one's callers, run only synchronously
        // inside it, on its thread. So the database is live and only shared-borrowed here;
        // read after `run`, the pointer would dangle.
        Some(load_cargo::answer(unsafe { &*db.0 }, req))
    }

    /// Asks the database `value` of `span`, and notes the question and its answer, or that
    /// the expansion is not to be kept when the span has an anchor or a context the input has
    /// not.
    fn ask_value(&mut self, value: Value, span: Span) -> Option<SubResponse> {
        // SAFETY: as in `ask`: `self.db` is `Some` only while `Session::run` holds the database
        // it points to, and this runs only inside that call, on its thread.
        let answer = self.db.map(|db| load_cargo::answer(unsafe { &*db.0 }, value.request(span)));
        let anchor = self.anchors.iter().position(|it| *it == span.anchor);
        let context = self.contexts.iter().position(|it| *it == span.ctx);
        match anchor.zip(context) {
            Some((anchor, context)) => {
                let (start, end) = (span.range.start().into(), span.range.end().into());
                self.asks.push(value, [start, end, anchor as u32, context as u32], answer.as_ref());
            }
            None => self.asked = true,
        }
        answer
    }

    fn span(&self, word: u64) -> Option<Span> {
        self.parts.get(word)
    }

    /// The word of `span`, which makes it one of the expansion's.
    fn pack(&mut self, span: Span) -> u64 {
        self.parts.pack(span)
    }

    /// Puts `tree` in [`State::buffer`] as the module reads a tree, for its `take`.
    fn give_tree(&mut self, tree: &tt::TopSubtree) -> i32 {
        self.buffer.clear();
        wire::relative(tree, &mut self.parts, &mut self.buffer);
        self.buffer.len() as i32
    }

    fn give(&mut self, text: Option<&str>) -> i32 {
        match text {
            Some(text) => {
                self.buffer.clear();
                self.buffer.extend_from_slice(text.as_bytes());
                self.buffer.len() as i32
            }
            None => -1,
        }
    }
}

/// Questions of spans, each with its answer, as a read set has them ([`Value::read`],
/// [`Value::said`]), in one text that is cleared, not freed, between expansions. An expansion
/// asks few, so one is found by a scan.
#[derive(Default)]
struct Questions {
    text: String,
    /// Where each question ends in `text`, then its answer: each starts where the one before
    /// ends.
    ends: Vec<[u32; 2]>,
}

impl Questions {
    fn clear(&mut self) {
        self.text.clear();
        self.ends.clear();
    }

    fn is_empty(&self) -> bool {
        self.ends.is_empty()
    }

    /// Notes the question `value` of the span of `fields` and what it was answered.
    fn push(&mut self, value: Value, fields: [u32; 4], answer: Option<&SubResponse>) {
        value.read(fields, &mut self.text);
        let read = self.text.len() as u32;
        value.said(answer, &mut self.text);
        self.ends.push([read, self.text.len() as u32]);
    }

    /// Notes `read` (as [`Value::read`] wrote it) with no answer yet.
    fn ask(&mut self, read: &str) {
        self.text.push_str(read);
        let end = self.text.len() as u32;
        self.ends.push([end, end]);
    }

    /// Notes `read` with what `db` answers it of the input of `shapes` ([`answer`]).
    fn answer(&mut self, read: &str, shapes: &Shapes, db: Option<&Db>) {
        self.text.push_str(read);
        let end = self.text.len() as u32;
        answer(read, shapes, db, &mut self.text);
        self.ends.push([end, self.text.len() as u32]);
    }

    fn iter(&self) -> impl Iterator<Item = (&str, &str)> {
        let mut start = 0;
        self.ends.iter().map(move |&[read, said]| {
            let it = (&self.text[start..read as usize], &self.text[read as usize..said as usize]);
            start = said as usize;
            it
        })
    }

    fn get(&self, read: &str) -> Option<&str> {
        self.iter().find(|it| it.0 == read).map(|it| it.1)
    }
}

/// A span query answered with a value rather than another span.
#[derive(Clone, Copy, PartialEq)]
enum Value {
    LineColumn,
    SourceText,
    ByteRange,
    File,
    LocalFile,
}

impl Value {
    const ALL: [(Value, char); 5] = [
        (Value::LineColumn, 'l'),
        (Value::SourceText, 't'),
        (Value::ByteRange, 'b'),
        (Value::File, 'f'),
        (Value::LocalFile, 'p'),
    ];

    fn request(self, span: Span) -> SubRequest {
        let (file_id, ast_id) = (span.anchor.file_id.as_u32(), span.anchor.ast_id.into_raw());
        let (start, end) = (span.range.start().into(), span.range.end().into());
        match self {
            Value::LineColumn => SubRequest::LineColumn { file_id, ast_id, offset: start },
            Value::SourceText => SubRequest::SourceText { file_id, ast_id, start, end },
            Value::ByteRange => SubRequest::ByteRange { file_id, ast_id, start, end },
            Value::File => SubRequest::FilePath { file_id: span.anchor.file_id.file_id().index() },
            Value::LocalFile => {
                SubRequest::LocalFilePath { file_id: span.anchor.file_id.file_id().index() }
            }
        }
    }

    /// Appends the question of the span of `shape` (as [`Shapes`] writes one) to `out` as a
    /// read set has it: a NUL first, which no path read has.
    fn read(self, [start, end, anchor, context]: [u32; 4], out: &mut String) {
        let letter = Value::ALL.iter().find(|it| it.0 == self).map_or('?', |it| it.1);
        _ = write!(out, "\0{letter}{start},{end},{anchor},{context}");
    }

    /// The question `read` (as [`Value::read`] wrote it) of the input that has the anchors and
    /// contexts of `shapes`.
    fn parse(read: &str, shapes: &Shapes) -> Option<(Value, Span)> {
        let mut chars = read.strip_prefix('\0')?.chars();
        let letter = chars.next()?;
        let value = Value::ALL.iter().find(|it| it.1 == letter)?.0;
        let mut fields = chars.as_str().split(',').map(|it| it.parse::<u32>().ok());
        let mut next = || fields.next().flatten();
        let (start, end, anchor, context) = (next()?, next()?, next()?, next()?);
        let span = Span {
            range: range(start, end),
            anchor: *shapes.anchors.get(anchor as usize)?,
            ctx: *shapes.contexts.get(context as usize)?,
        };
        Some((value, span))
    }

    /// Appends what a read set has of `answer` to `out`: the same for the same answer, short.
    fn said(self, answer: Option<&SubResponse>, out: &mut String) {
        match answer {
            Some(SubResponse::LineColumnResult { line, column }) => {
                _ = write!(out, "{line}:{column}");
            }
            Some(SubResponse::SourceTextResult { text: Some(text) }) => {
                blaze_format::hex::encode_into(&keyed::digest_bytes(text.as_bytes()), out);
            }
            Some(SubResponse::ByteRangeResult { range }) => {
                _ = write!(out, "{}..{}", range.start, range.end);
            }
            Some(SubResponse::FilePathResult { name }) => {
                blaze_format::hex::encode_into(&keyed::digest_bytes(name.as_bytes()), out);
            }
            Some(SubResponse::LocalFilePathResult { name: Some(name) }) => {
                blaze_format::hex::encode_into(&keyed::digest_bytes(name.as_bytes()), out);
            }
            _ => out.push('-'),
        }
    }
}

/// The module's environment as WASI's `environ_get` hands it over: each variable `NAME=value`
/// and a NUL, one after another, and where each starts. A copy and a table of pointers give it.
#[derive(Default)]
pub(crate) struct Environ {
    pub(crate) text: Vec<u8>,
    /// Where each variable starts in `text`: cargo sets fewer than this holds in place.
    pub(crate) starts: SmallVec<[u32; 24]>,
}

impl Environ {
    fn of(env: &[(&str, &str)]) -> Environ {
        let len = env.iter().map(|(name, value)| name.len() + value.len() + 2).sum();
        // alloc: setup, once an instance: the environment WASI hands the module, one buffer.
        let mut text = Vec::with_capacity(len);
        let mut starts = SmallVec::new();
        for (name, value) in env {
            starts.push(text.len() as u32);
            text.extend_from_slice(name.as_bytes());
            text.push(b'=');
            text.extend_from_slice(value.as_bytes());
            text.push(0);
        }
        Environ { text, starts }
    }
}

/// The memory of the instance `caller` runs in ([`State::memory`]).
pub(crate) fn memory(caller: &mut Caller<'_, State>) -> wasmtime::Result<Memory> {
    caller.data().memory.ok_or_else(|| wasmtime::Error::msg("the module exports no memory"))
}

/// The memory `instance` exports, which [`State::memory`] keeps.
fn exported_memory(
    instance: &Instance,
    store: &mut Store<State>,
    at: Option<&ModuleExport>,
) -> Option<Memory> {
    instance.get_module_export(&mut *store, at?)?.into_memory()
}

/// What a module that parses text outside an expansion (in a static's initializer or its list
/// export) is told: its tokens would have no call site to be spanned at.
const NO_CALL: &str =
    "a token stream can't be parsed outside a macro's expansion: it has no call site";

/// The module's memory and the host's state, lent at once, so what the module passes is read
/// where it is.
fn guest<'a>(caller: &'a mut Caller<'_, State>) -> wasmtime::Result<(&'a mut [u8], &'a mut State)> {
    let memory = memory(caller)?;
    Ok(memory.data_and_store_mut(caller))
}

/// The module's bytes at `ptr..ptr + len`, checked against its memory: a length is the
/// module's to choose.
fn guest_slice(data: &[u8], ptr: u32, len: u32) -> wasmtime::Result<&[u8]> {
    let start = ptr as usize;
    start
        .checked_add(len as usize)
        .and_then(|end| data.get(start..end))
        .ok_or_else(|| {
            // alloc: the message of a module that passed what is not its memory.
            wasmtime::Error::msg(format!("{len} bytes at {ptr:#x} are outside the module's memory"))
        })
}

/// `bytes` as text, each sequence that is not UTF-8 replaced.
fn guest_text(bytes: &[u8]) -> Cow<'_, str> {
    // alloc: none for UTF-8, which every `&str` a module passes is: the text is lent.
    String::from_utf8_lossy(bytes)
}

fn line_column(state: &mut State, span: Span) -> Option<(u32, u32)> {
    match state.ask_value(Value::LineColumn, span)? {
        SubResponse::LineColumnResult { line, column } => Some((line, column)),
        _ => None,
    }
}

/// Adds the host's functions to `linker`.
fn add_host(linker: &mut Linker<State>) -> wasmtime::Result<()> {
    linker.func_wrap(HOST, "def_site", |caller: Caller<'_, State>| caller.data().sites[0])?;
    linker.func_wrap(HOST, "call_site", |caller: Caller<'_, State>| caller.data().sites[1])?;
    linker.func_wrap(HOST, "mixed_site", |caller: Caller<'_, State>| caller.data().sites[2])?;
    linker.func_wrap(HOST, "span_join", |mut caller: Caller<'_, State>, a: u64, b: u64| {
        let state = caller.data_mut();
        let (Some(first), Some(second)) = (state.span(a), state.span(b)) else { return NONE };
        spans::join(first, second, |req| state.ask(req)).map_or(NONE, |it| state.pack(it))
    })?;
    linker.func_wrap(
        HOST,
        "span_resolved_at",
        |mut caller: Caller<'_, State>, a: u64, at: u64| {
            let state = caller.data_mut();
            let (Some(span), Some(at)) = (state.span(a), state.span(at)) else { return a };
            state.pack(Span { ctx: at.ctx, ..span })
        },
    )?;
    linker.func_wrap(
        HOST,
        "span_located_at",
        |mut caller: Caller<'_, State>, a: u64, b: u64| {
            let state = caller.data_mut();
            let (Some(span), Some(other)) = (state.span(a), state.span(b)) else { return b };
            state.pack(Span { ctx: span.ctx, ..other })
        },
    )?;
    linker.func_wrap(HOST, "span_parent", |mut caller: Caller<'_, State>, a: u64| {
        let state = caller.data_mut();
        let Some(span) = state.span(a) else { return NONE };
        let req = SubRequest::SpanParent {
            file_id: span.anchor.file_id.as_u32(),
            ast_id: span.anchor.ast_id.into_raw(),
            start: span.range.start().into(),
            end: span.range.end().into(),
            ctx: span.ctx.into_u32(),
        };
        match state.ask(req) {
            Some(SubResponse::SpanParentResult { parent_span: Some(it) }) => {
                state.pack(spanned(it.file_id, it.ast_id, it.start, it.end, it.ctx))
            }
            _ => NONE,
        }
    })?;
    linker.func_wrap(HOST, "span_source", |mut caller: Caller<'_, State>, a: u64| {
        let state = caller.data_mut();
        let Some(span) = state.span(a) else { return a };
        let req = SubRequest::SpanSource {
            file_id: span.anchor.file_id.as_u32(),
            ast_id: span.anchor.ast_id.into_raw(),
            start: span.range.start().into(),
            end: span.range.end().into(),
            ctx: span.ctx.into_u32(),
        };
        match state.ask(req) {
            Some(SubResponse::SpanSourceResult { file_id, ast_id, start, end, ctx }) => {
                state.pack(spanned(file_id, ast_id, start, end, ctx))
            }
            _ => a,
        }
    })?;
    linker.func_wrap(HOST, "span_start", |mut caller: Caller<'_, State>, a: u64| {
        let state = caller.data_mut();
        let Some(span) = state.span(a) else { return a };
        if span.anchor.ast_id == FIXUP_ERASED_FILE_AST_ID_MARKER {
            return a;
        }
        state.pack(Span { range: TextRange::empty(span.range.start()), ..span })
    })?;
    linker.func_wrap(HOST, "span_end", |mut caller: Caller<'_, State>, a: u64| {
        let state = caller.data_mut();
        let Some(span) = state.span(a) else { return a };
        if span.anchor.ast_id == FIXUP_ERASED_FILE_AST_ID_MARKER {
            return a;
        }
        state.pack(Span { range: TextRange::empty(span.range.end()), ..span })
    })?;
    linker.func_wrap(
        HOST,
        "span_subspan",
        |mut caller: Caller<'_, State>, a: u64, start: u64, end: u64| {
            let state = caller.data_mut();
            let Some(span) = state.span(a) else { return NONE };
            spans::subspan(span, start, end).map_or(NONE, |it| state.pack(it))
        },
    )?;
    linker.func_wrap(
        HOST,
        "span_pack",
        |mut caller: Caller<'_, State>, start: u32, len: u32, part: u32| {
            caller.data_mut().parts.pack_fields(start, len, part)
        },
    )?;
    linker.func_wrap(
        HOST,
        "span_unpack",
        |mut caller: Caller<'_, State>, a: u64, out: u32| -> wasmtime::Result<()> {
            let fields = caller.data().parts.fields(a);
            let mut bytes = [0u8; 12];
            for (at, field) in fields.into_iter().enumerate() {
                bytes[4 * at..4 * at + 4].copy_from_slice(&field.to_le_bytes());
            }
            let memory = memory(&mut caller)?;
            memory.write(&mut caller, out as usize, &bytes)?;
            Ok(())
        },
    )?;
    linker.func_wrap(
        HOST,
        "span_byte_range",
        |mut caller: Caller<'_, State>, a: u64, out: u32| -> wasmtime::Result<()> {
            let state = caller.data_mut();
            let Some(span) = state.span(a) else { return Ok(()) };
            let range = match state.ask_value(Value::ByteRange, span) {
                Some(SubResponse::ByteRangeResult { range }) => range,
                _ => usize::from(span.range.start())..usize::from(span.range.end()),
            };
            let mut bytes = [0u8; 8];
            bytes[..4].copy_from_slice(&(range.start as u32).to_le_bytes());
            bytes[4..].copy_from_slice(&(range.end as u32).to_le_bytes());
            let memory = memory(&mut caller)?;
            memory.write(&mut caller, out as usize, &bytes)?;
            Ok(())
        },
    )?;
    linker.func_wrap(HOST, "span_line", |mut caller: Caller<'_, State>, a: u64| {
        let state = caller.data_mut();
        let span = state.span(a);
        span.and_then(|span| line_column(state, span)).map_or(1, |(line, _)| line)
    })?;
    linker.func_wrap(HOST, "span_column", |mut caller: Caller<'_, State>, a: u64| {
        let state = caller.data_mut();
        let span = state.span(a);
        span.and_then(|span| line_column(state, span)).map_or(1, |(_, column)| column)
    })?;
    linker.func_wrap(HOST, "span_debug", |mut caller: Caller<'_, State>, a: u64| {
        let state = caller.data_mut();
        // What it says is of the database's numbering, which the key does not have.
        state.asked = true;
        let Some(span) = state.span(a) else { return -1 };
        state.buffer.clear();
        _ = write!(state.buffer, "{span:?}");
        state.buffer.len() as i32
    })?;
    linker.func_wrap(HOST, "span_file", |mut caller: Caller<'_, State>, a: u64| {
        let state = caller.data_mut();
        let Some(span) = state.span(a) else { return -1 };
        let name = match state.ask_value(Value::File, span) {
            Some(SubResponse::FilePathResult { name }) => Some(name),
            _ => None,
        };
        state.give(Some(name.as_deref().unwrap_or_default()))
    })?;
    linker.func_wrap(HOST, "span_local_file", |mut caller: Caller<'_, State>, a: u64| {
        let state = caller.data_mut();
        let Some(span) = state.span(a) else { return -1 };
        let name = match state.ask_value(Value::LocalFile, span) {
            Some(SubResponse::LocalFilePathResult { name }) => name,
            _ => None,
        };
        state.give(name.as_deref())
    })?;
    linker.func_wrap(HOST, "span_source_text", |mut caller: Caller<'_, State>, a: u64| {
        let state = caller.data_mut();
        let Some(span) = state.span(a) else { return -1 };
        let text = match state.ask_value(Value::SourceText, span) {
            Some(SubResponse::SourceTextResult { text }) => text,
            _ => None,
        };
        state.give(text.as_deref())
    })?;
    linker.func_wrap(
        HOST,
        "ts_from_str",
        |mut caller: Caller<'_, State>, ptr: u32, len: u32| -> wasmtime::Result<i32> {
            let (data, state) = guest(&mut caller)?;
            let text = guest_text(guest_slice(data, ptr, len)?);
            let Some(call_site) = state.span(state.sites[1]) else {
                return Ok(-1 - state.give(Some(NO_CALL)));
            };
            Ok(match lex::stream(&text, call_site) {
                Ok(trees) => state.give_tree(&wire::invisible(trees, call_site)),
                Err(e) => {
                    state.buffer.clear();
                    _ = write!(state.buffer, "failed to parse str to token stream: {e}");
                    -1 - state.buffer.len() as i32
                }
            })
        },
    )?;
    linker.func_wrap(
        HOST,
        "literal_from_str",
        |mut caller: Caller<'_, State>, ptr: u32, len: u32| -> wasmtime::Result<i32> {
            let (data, state) = guest(&mut caller)?;
            let text = guest_text(guest_slice(data, ptr, len)?);
            let Some(call_site) = state.span(state.sites[1]) else { return Ok(-1) };
            Ok(match lex::literal_from_str(&text, call_site) {
                Some(literal) => {
                    let trees = [Tree::Leaf(tt::Leaf::Literal(literal))];
                    state.give_tree(&wire::invisible(trees, call_site))
                }
                None => -1,
            })
        },
    )?;
    linker.func_wrap(
        HOST,
        "take",
        |mut caller: Caller<'_, State>, to: u32| -> wasmtime::Result<()> {
            let memory = memory(&mut caller)?;
            let buffer = std::mem::take(&mut caller.data_mut().buffer);
            let wrote = memory.write(&mut caller, to as usize, &buffer);
            // Given back emptied, its room kept for the next text or stream.
            let state = caller.data_mut();
            state.buffer = buffer;
            state.buffer.clear();
            Ok(wrote?)
        },
    )?;
    // These declarations do not grant access beyond the WASI sandbox.
    linker.func_wrap(HOST, "track_path", |_: u32, _: u32| {})?;
    linker.func_wrap(HOST, "track_env_var", |_: u32, _: u32| {})?;
    // As rust-analyzer's proc-macro server, which shows no diagnostic of a macro's.
    linker.func_wrap(HOST, "diagnostic", |_: u32, _: u32, _: u32, _: u64| {})?;
    linker.func_wrap(
        HOST,
        "output",
        |mut caller: Caller<'_, State>, ptr: u32, len: u32| -> wasmtime::Result<()> {
            let (data, state) = guest(&mut caller)?;
            let bytes = guest_slice(data, ptr, len)?;
            state.output.clear();
            state.output.extend_from_slice(bytes);
            state.gave = true;
            Ok(())
        },
    )?;
    linker.func_wrap(
        HOST,
        "panicked",
        |mut caller: Caller<'_, State>, ptr: u32, len: u32| -> wasmtime::Result<()> {
            let (data, state) = guest(&mut caller)?;
            let text = guest_text(guest_slice(data, ptr, len)?);
            state.panic = Some(text.into_owned());
            Ok(())
        },
    )?;
    linker.func_wrap(HOST, "retained", |mut caller: Caller<'_, State>, _bytes: u32| {
        caller.data_mut().retained = true;
    })?;
    Ok(())
}

/// A module of proc macros, compiled for this host.
pub struct Wasm {
    path: PathBuf,
    digest: [u8; 32],
    /// Source identity recorded with the module listing.
    identity: [u8; 32],
    /// Which engine it is compiled for, so what its instances are.
    kind: Kind,
    pre: InstancePre<State>,
    exports: Exports,
    /// How long Cranelift took to compile it.
    pub compiled_in: Duration,
    /// Whether the store has its machine code.
    kept: std::sync::atomic::AtomicBool,
}

impl fmt::Debug for Wasm {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Wasm").field("path", &self.path).finish_non_exhaustive()
    }
}

/// Where the exports an instance is used by are in its module, each looked up by name once a
/// module rather than once an instance.
#[derive(Clone, Copy)]
struct Exports {
    memory: Option<ModuleExport>,
    initialize: Option<ModuleExport>,
    alloc: Option<ModuleExport>,
    expand: Option<ModuleExport>,
}

impl Exports {
    fn of(module: &Module) -> Exports {
        Exports {
            memory: module.get_export_index("memory"),
            initialize: module.get_export_index("_initialize"),
            alloc: module.get_export_index(exports::ALLOC),
            expand: module.get_export_index(exports::EXPAND),
        }
    }
}

/// A macro of a module, as [`Wasm::macros`] lists them.
#[derive(Debug)]
pub struct Listed {
    pub kind: ProcMacroKind,
    pub name: String,
    pub helpers: Vec<String>,
}

/// What an expansion is given.
pub struct Call<'a> {
    pub subtree: &'a tt::TopSubtree,
    pub attrs: Option<&'a tt::TopSubtree>,
    pub env: &'a Env,
    pub def_site: Span,
    pub call_site: Span,
    pub mixed_site: Span,
}

/// How an expansion went: what it made, whether it was found kept, whether it left what it
/// allocated live for a later one, and the time.
pub struct Ran {
    pub output: Result<tt::TopSubtree, ProcMacroExpansionError>,
    pub found: bool,
    pub retained: bool,
    pub took: Duration,
}

impl Wasm {
    /// The module at `path`, compiled once a process, and its machine code kept in `store`
    /// under [`code_key`], which says the host it is for: unlike the module, the code is for
    /// one kind of host.
    pub fn at(path: &Path, store: Option<&blaze_store::Store>) -> Result<&'static Wasm, String> {
        Wasm::at_kind(path, store, Kind::Session)
    }

    /// As [`Wasm::at`], for instances of `kind`.
    fn at_kind(
        path: &Path,
        store: Option<&blaze_store::Store>,
        kind: Kind,
    ) -> Result<&'static Wasm, String> {
        // alloc: setup, once a module: its bytes, or the message of a file that can't be read.
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let known = Known { digest: keyed::sha256(&bytes), identity: None };
        Wasm::known(path, known, Some(&bytes), store, kind)
    }

    /// As [`Wasm::at`], of a module whose digest is known, for instances of `kind`: its bytes
    /// are read only if its machine code is not in the store. Found by its digest with no lock,
    /// no write and no allocation once compiled; a module is kept for the process, as the
    /// expanders it gives rust-analyzer are.
    fn known(
        path: &Path,
        known: Known,
        bytes: Option<&[u8]>,
        store: Option<&blaze_store::Store>,
        kind: Kind,
    ) -> Result<&'static Wasm, String> {
        // A module being compiled (`prepare_soon`) is waited for, not compiled again.
        static COMPILED: LazyLock<[Sharded<OnceLock<Result<Wasm, String>>>; 2]> =
            LazyLock::new(|| [Sharded::new(), Sharded::new()]);
        let compiled: &'static Sharded<_> = &COMPILED[kind as usize];
        // The digest is a SHA-256, which spreads over the shards as a content ID does.
        let at = compiled.intern(ContentId(known.digest), OnceLock::new);
        let once = compiled.get(at);
        let wasm = once.get_or_init(|| Wasm::compile(path, known, bytes, store, kind));
        let wasm = wasm.as_ref().map_err(Clone::clone)?;
        wasm.keep_code(store);
        Ok(wasm)
    }

    fn compile(
        path: &Path,
        Known { digest, identity }: Known,
        given: Option<&[u8]>,
        store: Option<&blaze_store::Store>,
        kind: Kind,
    ) -> Result<Wasm, String> {
        let start = Instant::now();
        // alloc: the message of a module that can't be compiled.
        let failed = |e: wasmtime::Error| format!("{}: {e:#}", path.display());
        let read = std::cell::OnceCell::new();
        let bytes = || module_bytes(path, &digest, given, &read);
        let engine = engine_of(kind);
        let key = code_key(&digest, engine);
        let stats = std::env::var_os("BLAZE_STATS").is_some();
        // Machine code is loaded only from the store, which checks what it gives against its
        // checksum: a file anyone can write is never run as code.
        let kept = store.and_then(|it| it.get_lasting(&key)).map(|code| {
            // SAFETY: `code` is what the store keeps under `key`, checked against its checksum
            // as it was read, and only `serialize` of this module on an engine of
            // `code_key`'s compatibility hash puts under `key`; Wasmtime refuses code of
            // another version, configuration or CPU. Other bytes would run as machine code.
            unsafe { Module::deserialize(engine, &code) }.map_err(|e| {
                // alloc: the message of machine code refused.
                format!(
                    "{}: the store's machine code of it, under a key of this engine's \
                     configuration, is refused: {e:#}",
                    path.display()
                )
            })
        });
        let kept = kept.transpose()?;
        let (module, was_kept) = match kept {
            Some(module) => (module, true),
            None => {
                let module = Module::new(engine, bytes()?).map_err(failed)?;
                if stats {
                    eprintln!(
                        "blaze: Cranelift compiled the proc-macro module {} in {:.1?}",
                        path.display(),
                        start.elapsed(),
                    );
                }
                if let Some(store) = store {
                    let code = module.serialize().map_err(failed)?;
                    store.insert_lasting(&key, &code);
                    store.flush();
                }
                (module, store.is_some())
            }
        };
        let pre = linked(&module).map_err(failed)?;
        let exports = Exports::of(&module);
        let identity = match identity {
            Some(identity) => identity,
            None => made_of(path, bytes()?).unwrap_or(digest),
        };
        let compiled_in = start.elapsed();
        let kept = was_kept.into();
        // alloc: setup, once a module: its path, for what it says of itself.
        let path = path.to_owned();
        let wasm = Wasm { path, digest, identity, kind, pre, exports, compiled_in, kept };
        wasm.keep_code(store);
        count(Counted::Loaded, start.elapsed());
        Ok(wasm)
    }

    /// Keeps its machine code in `store`, if it has not: a module compiled before the daemon
    /// told the store the toolchain is kept once it has.
    fn keep_code(&self, store: Option<&blaze_store::Store>) {
        let Some(store) = store else { return };
        // A plain load first: every lookup of a module passes here, and a swap would write
        // the shared line each time.
        if self.kept.load(Ordering::Relaxed) || self.kept.swap(true, Ordering::Relaxed) {
            return;
        }
        let module = self.pre.module();
        if let Ok(code) = module.serialize() {
            store.insert_lasting(&code_key(&self.digest, module.engine()), &code);
            store.flush();
            if std::env::var_os("BLAZE_STATS").is_some() {
                eprintln!("blaze: the store keeps the machine code of {}", self.path.display());
            }
        }
    }

    /// An instance, its C library set up: a module linked as a library (a WASI reactor) is
    /// started by its `_initialize`.
    fn instance(&self, store: &mut Store<State>) -> wasmtime::Result<Instance> {
        let instance = self.pre.instantiate(&mut *store)?;
        store.data_mut().memory = exported_memory(&instance, store, self.exports.memory.as_ref());
        let initialize =
            self.exports.initialize.and_then(|at| instance.get_module_export(&mut *store, &at));
        let initialize = initialize.and_then(Extern::into_func).map(|it| it.typed::<(), ()>(&*store));
        if let Some(Ok(initialize)) = initialize {
            initialize.call(&mut *store, ())?;
        }
        Ok(instance)
    }

    /// The module's macros.
    pub fn macros(&self) -> Result<Vec<Listed>, String> {
        Ok(listed(&self.list()?))
    }

    /// What its list export says: a line per macro, kind, name and helpers by tabs.
    fn list(&self) -> Result<String, String> {
        // alloc: the message of a module whose list can't be had.
        let failed = |e: wasmtime::Error| format!("{e:#}");
        // Given back after the store, which is dropped first.
        let _slot = Slot::for_kind(self.kind);
        let mut store = Store::new(self.pre.module().engine(), State::empty());
        store.limiter(|state| &mut state.limits);
        let instance = self.instance(&mut store).map_err(failed)?;
        let list = instance.get_typed_func::<(), ()>(&mut store, exports::LIST).map_err(failed)?;
        list.call(&mut store, ()).map_err(failed)?;
        let text = std::mem::take(&mut store.data_mut().output);
        // alloc: the message of a list that is not text.
        String::from_utf8(text).map_err(|e| e.to_string())
    }
}

/// The macros of a module's list ([`Wasm::list`]): setup, once a module.
fn listed(text: &str) -> Vec<Listed> {
    // alloc: setup, once a module: its macros, a line each.
    let mut macros = Vec::with_capacity(text.lines().count());
    for line in text.lines() {
        let mut fields = line.split('\t');
        let kind = match fields.next() {
            Some("0") => ProcMacroKind::CustomDerive,
            Some("1") => ProcMacroKind::Attr,
            _ => ProcMacroKind::Bang,
        };
        // alloc: setup, as above: the macro's name.
        let name = fields.next().unwrap_or_default().to_owned();
        let helpers = fields.next().unwrap_or_default();
        let helpers = helpers.split(',').filter(|it| !it.is_empty()).map(str::to_owned);
        // alloc: setup, as above: the derive's helper attributes.
        macros.push(Listed { kind, name, helpers: helpers.collect() });
    }
    macros
}

/// A module instance for one crate's macro expansions in one database revision.
/// Guest tokens can outlive a call, so their spans and WASI descriptors live here too.
pub struct Session {
    store: Store<State>,
    instance: Instance,
    root: Option<PathBuf>,
    failed: bool,
    /// The exports an expansion calls, found as the instance is made.
    calls: Option<Calls>,
    /// The pool's slot a fresh instance is in, given back after `store` is dropped: fields
    /// drop in the order they are declared.
    _slot: Option<Slot>,
}

/// The module's exports an expansion calls.
struct Calls {
    alloc: TypedFunc<u32, u32>,
    expand: TypedFunc<(u32, u32, u32, u32, u32), ()>,
    memory: Memory,
}

impl Calls {
    fn of(instance: &Instance, store: &mut Store<State>) -> Result<Calls, ProcMacroExpansionError> {
        // alloc: the message of a module without the exports an expansion calls.
        let system = |e: wasmtime::Error| ProcMacroExpansionError::System(format!("{e:#}"));
        let alloc = instance.get_typed_func(&mut *store, exports::ALLOC).map_err(system)?;
        let expand = instance.get_typed_func(&mut *store, exports::EXPAND).map_err(system)?;
        let memory = instance.get_memory(&mut *store, "memory").ok_or_else(|| {
            // alloc: as above.
            ProcMacroExpansionError::System("the module exports no memory".to_owned())
        })?;
        Ok(Calls { alloc, expand, memory })
    }

    /// The calls of `instance`, found where `exports` says they are with no lookup by name:
    /// `None` when one is missing, which [`Calls::of`] then names.
    fn known(instance: &Instance, store: &mut Store<State>, exports: &Exports) -> Option<Calls> {
        let mut func = |at: Option<ModuleExport>| {
            instance.get_module_export(&mut *store, &at?).and_then(Extern::into_func)
        };
        let (alloc, expand) = (func(exports.alloc)?, func(exports.expand)?);
        let alloc: TypedFunc<u32, u32> = alloc.typed(&*store).ok()?;
        let expand: TypedFunc<(u32, u32, u32, u32, u32), ()> = expand.typed(&*store).ok()?;
        Some(Calls { alloc, expand, memory: store.data().memory? })
    }
}

impl Wasm {
    pub fn session(&self, env: &Env) -> Result<Session, ProcMacroExpansionError> {
        let (vars, root) = guest_env(env);
        let mut state = State::empty();
        state.files = wasi::Files::new(root);
        state.environ = Environ::of(&vars);
        // alloc: setup, once a session: where its outputs name the package.
        self.instance_with(state, root.map(Path::to_path_buf))
    }

    /// A fresh instance whose host side starts as `state`, in a slot of the pool when this
    /// module's instances take one ([`Kind`]).
    fn instance_with(
        &self,
        state: State,
        root: Option<PathBuf>,
    ) -> Result<Session, ProcMacroExpansionError> {
        // Declared before the store, so dropped after it when the instance can't start.
        let slot = Slot::for_kind(self.kind);
        let mut store = Store::new(self.pre.module().engine(), state);
        store.limiter(|state| &mut state.limits);
        let instance = self
            .instance(&mut store)
            // alloc: the message of a module that can't be started.
            .map_err(|e| ProcMacroExpansionError::System(format!("{e:#}")))?;
        let calls = Calls::known(&instance, &mut store, &self.exports);
        Ok(Session { store, instance, root, failed: false, calls, _slot: slot })
    }

    /// Expands the macro at `index` of [`Wasm::macros`] in a fresh instance: what is kept of
    /// it when the store has it, else by running it, and keeps what it made unless it can't be
    /// kept ([`expand`]).
    pub fn expand(
        &self,
        index: u32,
        listed: &Listed,
        db: Option<&Db>,
        call: &Call<'_>,
    ) -> Ran {
        expand(blaze_expand::keeping(), &code(&self.digest), &|| Ok(self), index, listed, db, call)
    }
}

impl Session {
    /// Executes every call: returning cached tokens would skip changes to guest statics.
    pub fn expand(
        &mut self,
        index: u32,
        listed: &Listed,
        db: Option<&Db>,
        call: &Call<'_>,
    ) -> Ran {
        let start = Instant::now();
        if self.failed {
            return Ran {
                output: Err(ProcMacroExpansionError::System(
                    // alloc: the message of a session that failed.
                    "an earlier expansion failed; the macro session cannot continue in this revision".to_owned(),
                )),
                found: false,
                retained: false,
                took: start.elapsed(),
            };
        }
        let state = self.store.data_mut();
        // The words the module holds from earlier calls stay those of their spans, so the
        // session's parts are only added to.
        state.sites = [call.def_site, call.call_site, call.mixed_site].map(|span| state.pack(span));
        state.buffer.clear();
        state.output.clear();
        state.gave = false;
        state.panic = None;
        state.retained = false;
        state.files.output.clear();
        // Nothing a session makes is kept, so what it read is of no use.
        state.files.forget_traced();
        let mut input = Reused::take();
        let mut attr = Reused::take();
        wire::relative(call.subtree, &mut state.parts, &mut input.0);
        // An attribute written without arguments is given as no bytes, an empty stream.
        let attr = match (call.attrs, listed.kind) {
            (Some(it), _) => {
                wire::relative(it, &mut state.parts, &mut attr.0);
                Some(attr)
            }
            (None, ProcMacroKind::Attr) => Some(attr),
            (None, _) => None,
        };
        let output = self.run(index, db, &input, attr.as_deref()).and_then(|()| {
            let state = self.store.data();
            let tree = wire::read(&state.output, &state.parts)
                // alloc: the message of an output refused.
                .map_err(|e| ProcMacroExpansionError::System(format!("`{}`: {e}", listed.name)))?;
            Ok(match &self.root {
                Some(root) => placed(tree, root),
                None => tree,
            })
        });
        if output.is_err() {
            self.failed = true;
        }
        let took = start.elapsed();
        count(Counted::Ran, took);
        Ran { output, found: false, retained: self.store.data().retained, took }
    }

    /// Runs the macro at `index` on `input` and `attr`, leaving what it gave in the state's
    /// [`State::output`].
    fn run(
        &mut self,
        index: u32,
        db: Option<&Db>,
        input: &[u8],
        attr: Option<&[u8]>,
    ) -> Result<(), ProcMacroExpansionError> {
        // `run` sets `db` to `None` before it returns or resumes an unwind, so the pointer is
        // never read after the borrow it came from ends, and the session keeps no database
        // between calls.
        self.store.data_mut().db = db.map(|db| SessionDb(std::ptr::from_ref(db)));
        let ran = std::panic::catch_unwind(AssertUnwindSafe(|| {
            // alloc: the message of a module that trapped.
            let system = |e: wasmtime::Error| ProcMacroExpansionError::System(format!("{e:#}"));
            let calls = match self.calls.take() {
                Some(calls) => calls,
                None => Calls::of(&self.instance, &mut self.store)?,
            };
            let Calls { alloc, expand, memory } = &calls;
            let mut put = |bytes: &[u8]| -> Result<u32, ProcMacroExpansionError> {
                let at = alloc.call(&mut self.store, bytes.len() as u32).map_err(system)?;
                memory.write(&mut self.store, at as usize, bytes).map_err(|e| system(e.into()))?;
                Ok(at)
            };
            let input_at = put(input)?;
            let attr_at = attr.map(&mut put).transpose()?.unwrap_or(0);
            let attr_len = attr.map_or(0, |it| it.len() as u32);
            let called =
                expand.call(&mut self.store, (index, attr_at, attr_len, input_at, input.len() as u32));
            self.calls = Some(calls);
            called.map_err(|trap| {
                let state = self.store.data_mut();
                match state.panic.take() {
                    Some(message) => ProcMacroExpansionError::Panic(message),
                    // alloc: the message of a module that trapped.
                    None => ProcMacroExpansionError::System(format!(
                        "{trap:#}\n{}",
                        guest_text(&state.files.output),
                    )),
                }
            })?;
            match self.store.data().gave {
                true => Ok(()),
                // alloc: the message of a macro that gave nothing.
                false => Err(ProcMacroExpansionError::System("the macro gave no output".to_owned())),
            }
        }));
        self.store.data_mut().db = None;
        match ran {
            Ok(result) => result,
            Err(payload) => {
                self.failed = true;
                std::panic::resume_unwind(payload)
            }
        }
    }
}

/// What the store keeps the machine code of the module of `digest` under: the module's bytes
/// and what Wasmtime says code must match to load (its version, its configuration, the
/// target and the CPU features it compiled for, [`CPU_FEATURES`] rather than the host's),
/// and nothing of the checkout, the build of blaze or the toolchain, so every checkout and
/// every host of the same kind shares it. `engine` is the one the code is for.
fn code_key(digest: &[u8; 32], engine: &Engine) -> [u8; 32] {
    use std::hash::Hash as _;
    struct Fields<'a>(&'a mut keyed::Key);
    impl std::hash::Hasher for Fields<'_> {
        fn write(&mut self, bytes: &[u8]) {
            self.0.field(bytes);
        }
        fn finish(&self) -> u64 {
            0
        }
    }
    let mut key = keyed::Key::new("wasmtime machine code of a proc-macro module 2");
    key.field(digest).field(std::env::consts::ARCH.as_bytes());
    engine.precompile_compatibility_hash().hash(&mut Fields(&mut key));
    key.done()
}

/// The kind of the instances of the module of `digest` in this process, as a load decides it
/// ([`Lazy::new`]): what readies it compiles it for the same engine, so the load waits for that
/// compile rather than making another.
fn kind_for(digest: &[u8; 32]) -> Kind {
    Kind::of(verdict(digest))
}

/// Whether `store` has machine code of the module of `digest` for this process's engine.
fn has_code(store: &blaze_store::Store, digest: &[u8; 32]) -> bool {
    store.has_lasting(&code_key(digest, engine_of(kind_for(digest))))
}

/// Writes `bytes` to `path` whole or not at all: a process that reads it while it is written
/// sees the old file or none.
fn write_whole(path: &Path, bytes: &[u8]) {
    // alloc: setup, once a module: the name it is written under first.
    let new = path.with_extension(format!("{}.new", std::process::id()));
    if std::fs::write(&new, bytes).is_err() || std::fs::rename(&new, path).is_err() {
        _ = std::fs::remove_file(&new);
    }
}

/// Readies the module at `path` for the processes that expand with it, in the build that
/// made it: compiles it with Cranelift (or finds its machine code in `store`), keeps the code
/// in `store`, and writes the list of its macros beside it, so that a load with the store
/// compiles nothing and reads the list without running it.
pub fn prepare(path: &Path, store: Option<&blaze_store::Store>) -> Result<(), String> {
    let start = Instant::now();
    if let Some((known, _)) = listing(path)
        && store.is_some_and(|it| has_code(it, &known.digest))
    {
        return Ok(());
    }
    // alloc: setup, once a module: its bytes, or the message of a file that can't be read.
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let known = Known { digest: keyed::sha256(&bytes), identity: None };
    let wasm = Wasm::known(path, known, Some(&bytes), store, kind_for(&known.digest))?;
    list(path, wasm, &wasm.list()?);
    if std::env::var_os("BLAZE_STATS").is_some() {
        eprintln!(
            "blaze: the proc-macro module {} ready in {:.1?}",
            path.display(),
            start.elapsed()
        );
    }
    Ok(())
}

/// As [`prepare`], but a module that carries its list of macros ([`exports::LIST_SECTION`]) is
/// compiled on a thread of its own: its list is written now, and the first of its macros to
/// expand in this process waits for the compile.
pub fn prepare_soon(path: &Path, store: Option<&'static blaze_store::Store>) -> Result<(), String> {
    if let Some((known, _)) = listing(path)
        && store.is_some_and(|it| has_code(it, &known.digest))
    {
        return Ok(());
    }
    // alloc: setup, once a module: its bytes, or the message of a file that can't be read.
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let Some(text) = carried_list(&bytes) else { return prepare(path, store) };
    let digest = keyed::sha256(&bytes);
    let identity = made_of(path, &bytes).unwrap_or(digest);
    list_known(path, &digest, &identity, &text);
    // alloc: setup, once a module: its path, for the thread that compiles it.
    let path = path.to_owned();
    let known = Known { digest, identity: Some(identity) };
    let kind = kind_for(&digest);
    std::thread::spawn(move || {
        let start = Instant::now();
        let done = Wasm::known(&path, known, Some(&bytes), store, kind);
        if std::env::var_os("BLAZE_STATS").is_some() {
            // alloc: the edge: what the statistics print.
            let how = done.map_or_else(|e| format!("failed: {e}"), |_| "ready".to_owned());
            eprintln!(
                "blaze: the proc-macro module {} {how} in {:.1?}",
                path.display(),
                start.elapsed()
            );
        }
    });
    Ok(())
}

/// `module` linked to the host: WASI preview 1, the proc-macro imports, and an error for each
/// import it has that neither gives.
fn linked(module: &Module) -> wasmtime::Result<InstancePre<State>> {
    let mut linker = Linker::new(module.engine());
    wasi::add(&mut linker)?;
    add_host(&mut linker)?;
    wasi::rest(&mut linker, module)?;
    linker.instantiate_pre(module)
}

/// Whether `bytes` is a module the host can run: compiled by Cranelift and linked as
/// [`Wasm::at`] links it, but neither run, kept in memory, nor its machine code written.
pub fn check_module(bytes: &[u8]) -> Result<(), String> {
    // alloc: the message of a module refused.
    let failed = |e: wasmtime::Error| format!("{e:#}");
    let module = Module::new(engine(), bytes).map_err(failed)?;
    linked(&module).map(drop).map_err(failed)
}

/// The tree of `bytes`, a tree as a module writes one, whose span parts 0, 1 and 2 are those
/// of `sites` (def site, call site, mixed site), each distinct one given the next index.
pub fn decode(bytes: &[u8], sites: [Span; 3]) -> Result<tt::TopSubtree, String> {
    let mut parts = Parts::default();
    for site in sites {
        parts.pack(site);
    }
    wire::read(bytes, &parts)
}

/// What the module of `bytes` carries in its [`exports::LIST_SECTION`] custom section, read
/// without compiling it: `None` for a module built before modules carried one.
pub fn carried_list(bytes: &[u8]) -> Option<String> {
    fn leb(bytes: &[u8], at: &mut usize) -> Option<usize> {
        blaze_format::leb::read_u32(bytes, at).ok().map(|it| it as usize)
    }
    if bytes.get(..4)? != b"\0asm" {
        return None;
    }
    let mut at = 8;
    while at < bytes.len() {
        let id = bytes[at];
        at += 1;
        let size = leb(bytes, &mut at)?;
        let end = at.checked_add(size).filter(|it| *it <= bytes.len())?;
        if id == 0 {
            let mut named = at;
            let len = leb(bytes, &mut named)?;
            let name = bytes.get(named..named.checked_add(len)?)?;
            if name == exports::LIST_SECTION.as_bytes() {
                let text = std::str::from_utf8(bytes.get(named + len..end)?).ok()?;
                // alloc: setup, once a module: the list it carries, which the caller keeps.
                return Some(text.to_owned());
            }
        }
        at = end;
    }
    None
}

/// What is known of a module without reading it: its digest and source identity.
#[derive(Clone, Copy)]
struct Known {
    digest: [u8; 32],
    identity: Option<[u8; 32]>,
}

/// The bytes of the module at `path` of `digest`: `given`, or read into `read` once.
fn module_bytes<'a>(
    path: &Path,
    digest: &[u8; 32],
    given: Option<&'a [u8]>,
    read: &'a std::cell::OnceCell<Result<Vec<u8>, String>>,
) -> Result<&'a [u8], String> {
    if let Some(given) = given {
        return Ok(given);
    }
    let read = read.get_or_init(|| {
        // alloc: setup, once a module: its bytes, or the message of a file that can't be read.
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        match keyed::sha256(&bytes) == *digest {
            true => Ok(bytes),
            // alloc: the message of a module that changed.
            false => Err(format!("{} changed while it was loaded", path.display())),
        }
    });
    read.as_deref().map_err(Clone::clone)
}

/// What tells that the module at `path` and the file beside it that says what it was built of
/// are the files a list was written for: each one's length, time of writing and inode. Both
/// are written whole under a new name when they change.
fn stamp(path: &Path) -> Option<String> {
    let one = |path: &Path, out: &mut String| -> Option<()> {
        let Ok(meta) = std::fs::metadata(path) else {
            out.push('-');
            return Some(());
        };
        let written = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(&meta);
        #[cfg(not(unix))]
        let inode = 0;
        _ = write!(out, "{}.{}.{inode}", meta.len(), written.as_nanos());
        Some(())
    };
    // alloc: setup, once a module: the name of the file beside it.
    let mut made_of = path.as_os_str().to_owned();
    made_of.push(".made-of");
    // alloc: setup, as above: the stamp, which the list file keeps.
    let mut out = String::with_capacity(96);
    one(path, &mut out)?;
    out.push(':');
    one(Path::new(&made_of), &mut out)?;
    Some(out)
}

/// What the list file beside the module at `path` knows of it, and its list of macros, when
/// it was written for the files there now ([`stamp`]): then the module need not be read.
fn listing(path: &Path) -> Option<(Known, String)> {
    let mut text = std::fs::read_to_string(list_file(path)).ok()?;
    let (digest, rest) = text.split_once('\n')?;
    let (head, list) = rest.split_once('\n').unwrap_or((rest, ""));
    let (identity, stamped) = head.strip_prefix("@ ")?.split_once(' ')?;
    (stamp(path)? == stamped).then_some(())?;
    let known = Known { digest: blaze_format::hex::decode::<32>(digest)?, identity: Some(blaze_format::hex::decode::<32>(identity)?) };
    // The list is the text's tail: the head is cut off, and the text is the list.
    let at = text.len() - list.len();
    text.drain(..at);
    Some((known, text))
}

/// Writes the list file of the module at `path`, compiled as `wasm`, whose macros `text` lists.
fn list(path: &Path, wasm: &Wasm, text: &str) {
    list_known(path, &wasm.digest, &wasm.identity, text);
}

fn list_known(path: &Path, digest: &[u8; 32], identity: &[u8; 32], text: &str) {
    let Some(stamp) = stamp(path) else { return };
    // alloc: setup, once a module: the list file's text.
    let mut file = String::with_capacity(2 * 64 + stamp.len() + text.len() + 5);
    blaze_format::hex::encode_into(digest, &mut file);
    file.push_str("\n@ ");
    blaze_format::hex::encode_into(identity, &mut file);
    file.push(' ');
    file.push_str(&stamp);
    file.push('\n');
    file.push_str(text);
    write_whole(&list_file(path), file.as_bytes());
}

/// What the module of `bytes` at `path` was built of, as the file beside it says, when that
/// file was written for these bytes.
fn made_of(path: &Path, bytes: &[u8]) -> Option<[u8; 32]> {
    // alloc: setup, once a module: the name of the file beside it.
    let mut named = path.as_os_str().to_owned();
    named.push(".made-of");
    let text = std::fs::read_to_string(Path::new(&named)).ok()?;
    let (key, file) = text.trim_end().split_once(' ')?;
    let mut identity = keyed::Key::new("a proc-macro module built of");
    identity.field(key.as_bytes());
    blaze_format::hex::is_encoding(file, &keyed::digest_bytes(bytes)).then(|| identity.done())
}

/// What a module is told of a crate's environment ([`guest_env`]), on the stack: cargo sets
/// fewer for a compilation.
type GuestEnv<'a> = SmallVec<[(&'a str, &'a str); 24]>;

/// What a module is told of its environment: cargo's variables for a crate's compilation
/// that the crate's env has, each with what it says on every machine, sorted. No other
/// variable, of the crate's or of the process's. And the package directory of the crate, from
/// its `CARGO_MANIFEST_DIR` when that is absolute.
fn guest_env(env: &Env) -> (GuestEnv<'_>, Option<&Path>) {
    let mut root = None;
    let mut out = GuestEnv::new();
    for (name, value) in env.iter() {
        let value = match name {
            "CARGO_MANIFEST_DIR" => {
                root = Some(Path::new(value)).filter(|it| it.is_absolute());
                wasi::PACKAGE
            }
            "CARGO_MANIFEST_PATH" => MANIFEST_PATH,
            "OUT_DIR" => wasi::OUT_DIR,
            "CARGO_CRATE_NAME" | "CARGO_PRIMARY_PACKAGE" | "CARGO_BIN_NAME" => value,
            _ if name.starts_with("CARGO_PKG_") => value,
            _ => continue,
        };
        out.push((name, value));
    }
    out.sort_unstable();
    (out, root)
}

/// Where a module's manifest is: [`wasi::PACKAGE`]'s `Cargo.toml`.
const MANIFEST_PATH: &str = "/blaze/package/Cargo.toml";

/// `tree` with the package's path where the module put [`wasi::PACKAGE`] in a string, as
/// a path `include_str!` is to read: itself when no string has it.
fn placed(tree: tt::TopSubtree, root: &Path) -> tt::TopSubtree {
    let string = |kind: tt::LitKind| matches!(kind, tt::LitKind::Str | tt::LitKind::StrRaw(_));
    let mut found = false;
    tree.token_trees().for_each_token(|token, _| {
        if let tt::FlatToken::Literal { sym, kind, .. } = token {
            found |= string(kind) && sym.as_str().contains(wasi::PACKAGE);
        }
    });
    if !found {
        return tree;
    }
    let root = root.to_string_lossy();
    // alloc: a rebuilt tree, only for an output that names the package, which is rare.
    let flat = tree
        .as_token_trees()
        .iter_flat_tokens()
        .map(|token| match token {
            tt::TokenTree::Leaf(tt::Leaf::Literal(literal))
                if string(literal.kind) && literal.text().contains(wasi::PACKAGE) =>
            {
                let (text, suffix) = literal.text_and_suffix();
                let text = text.replace(wasi::PACKAGE, &root);
                let literal = tt::Literal::new(&text, literal.span, literal.kind, suffix);
                tt::TokenTree::Leaf(tt::Leaf::Literal(literal))
            }
            other => other,
        })
        // alloc: only an output with a string naming the package, rebuilt with the package's place.
        .collect();
    tt::TopSubtree::from_serialized(flat)
}

/// The span parts of an expansion's input as its key has them: which of the input's anchors
/// and contexts each has, numbered as they first appear, then the def, call and mixed sites'
/// words, whose ranges the input's bytes don't have. A kept output's parts are written the
/// same way, so the next call, with other anchors and contexts in the same places, is given
/// them back as its own.
#[derive(Default)]
struct Shapes {
    anchors: Vec<SpanAnchor>,
    contexts: Vec<SyntaxContext>,
    bytes: Vec<u8>,
}

impl Shapes {
    #[cfg(test)]
    fn of(parts: &[Part], sites: [u64; 3]) -> Shapes {
        let mut shapes = Shapes::default();
        shapes.fill(parts, sites);
        shapes
    }

    /// Makes these the shapes of `parts` and the three sites' words, keeping the room.
    fn fill(&mut self, parts: &[Part], sites: [u64; 3]) {
        self.anchors.clear();
        self.contexts.clear();
        self.bytes.clear();
        for &(anchor, ctx) in parts {
            let anchor = blaze_expand::number(&mut self.anchors, anchor);
            let context = blaze_expand::number(&mut self.contexts, ctx);
            for it in [anchor, context] {
                self.bytes.extend_from_slice(&u32::to_le_bytes(it));
            }
        }
        for site in sites {
            self.bytes.extend_from_slice(&site.to_le_bytes());
        }
    }

    /// What is kept of an output: the shapes of every part of the expansion, then the output.
    /// `None` when one has an anchor or a context the input has not.
    fn written(&self, parts: &Parts, output: &[u8]) -> Option<Vec<u8>> {
        let mut value = Vec::with_capacity(4 + 8 * parts.len() + output.len());
        value.extend_from_slice(&(parts.len() as u32).to_le_bytes());
        for (anchor, ctx) in parts.all() {
            let anchor = self.anchors.iter().position(|it| it == anchor)? as u32;
            let context = self.contexts.iter().position(|it| it == ctx)? as u32;
            for it in [anchor, context] {
                value.extend_from_slice(&u32::to_le_bytes(it));
            }
        }
        value.extend_from_slice(output);
        Some(value)
    }

    /// The output kept as [`Shapes::written`] wrote it, its parts this input's, put in `parts`.
    /// A tree off a 4-byte boundary is copied into `room`.
    fn read(&self, value: &[u8], parts: &mut Parts, room: &mut Vec<u8>) -> Result<tt::TopSubtree, String> {
        let count = value.get(..4).map(|it| u32::from_le_bytes(it.try_into().unwrap()) as usize);
        let table = count.and_then(|count| value.get(4..4 + count.checked_mul(8)?));
        let Some(table) = table else { return Err("its table of span parts is cut short".into()) };
        parts.clear();
        for (at, shape) in table.as_chunks::<8>().0.iter().enumerate() {
            let field = |at: usize| u32::from_le_bytes(shape[at..at + 4].try_into().unwrap());
            let anchor = self.anchors.get(field(0) as usize);
            let ctx = self.contexts.get(field(4) as usize);
            let Some((&anchor, &ctx)) = anchor.zip(ctx) else {
                // alloc: the message of a kept output refused.
                return Err(format!("its span part {at} is of no anchor and context of the input"));
            };
            if parts.part(anchor, ctx) as usize != at {
                // alloc: the message of a kept output refused.
                return Err(format!("its span part {at} is an earlier one again"));
            }
        }
        wire::read_in(&value[4 + table.len()..], parts, room)
    }
}

/// The content ID a module's expansions are keyed by as their code ([`RunKey::code`]): of its
/// bytes, by their SHA-256, which a load knows without reading them.
pub fn code(digest: &[u8; 32]) -> ContentId {
    ContentId::of(MODULE_CODE, digest)
}

/// What [`code`] is made with.
const MODULE_CODE: &str = "blaze v2 wasm proc-macro module 1";

/// What an expansion read, as its key has it: what the host traced it reading of its package
/// ([`wasi::Files::traced`]), and each question it asked of a span of its input with its
/// answer, as a variable named by the question ([`Value::read`], which no variable's name
/// can be) whose value is the answer. `None` when it can't be kept by what it read: it
/// learned of a span what neither its key nor a question says, or met a file it could not
/// read.
fn what_it_read(state: &mut State) -> Option<Traced> {
    if state.asked || state.files.unreadable.is_some() {
        return None;
    }
    let mut traced = state.files.take_traced();
    traced.env.reserve(state.asks.ends.len());
    for (read, said) in state.asks.iter() {
        // alloc: the read set kept with the output, the result's own.
        traced.env.push((read.into(), Some(said.into())));
    }
    state.asks.clear();
    blaze_usercode::trace::normalize(&mut traced);
    Some(traced)
}

/// The inputs of a module's expansion as they are now ([`index::find`]): its package's files
/// on the machine, by the paths the module has for them, its environment, and the database's
/// answers to its questions of a span. It asks the database nothing, as `find` runs with the
/// store's guard held: a question it has no answer to yet is noted in `unanswered`, and
/// [`find_answered`] asks it once `find` has returned and looks again.
struct Now<'a> {
    root: Option<&'a Path>,
    machine: inputs::Now<'a>,
    /// The module's environment, sorted by name ([`guest_env`]), borrowed from the call's.
    env: &'a [(&'a str, &'a str)],
    /// The answers so far, by question as a read set has it ([`Value::read`]).
    answers: &'a Questions,
    unanswered: &'a mut Questions,
    /// Where the package's path asked of last is on this machine.
    real: &'a mut PathBuf,
    /// The environment owned, for a shape that read all of it ([`Inputs::env_all`]).
    all: &'a mut Vec<(OsString, OsString)>,
}

/// What a lookup of an expansion keeps between calls on a thread, cleared and not freed:
/// [`Now`]'s answers, its questions with none yet, its path on the machine and its
/// environment owned.
#[derive(Default)]
struct Room {
    answers: Questions,
    unanswered: Questions,
    real: PathBuf,
    all: Vec<(OsString, OsString)>,
}

/// Appends what a read set has for the question `read` ([`Value::read`]) of the input of
/// `shapes`, as `db` answers it, to `out`: no database answers nothing ([`Value::said`]).
fn answer(read: &str, shapes: &Shapes, db: Option<&Db>, out: &mut String) {
    match Value::parse(read, shapes) {
        Some((value, span)) => {
            let answer = db.map(|db| load_cargo::answer(db, value.request(span)));
            value.said(answer.as_ref(), out);
        }
        None => out.push('?'),
    }
}

/// Puts where the package's `path` is on this machine in `real`: `false` for a module given no
/// package, which finds nothing there.
fn real(root: Option<&Path>, path: &Path, real: &mut PathBuf) -> Result<bool, Unreadable> {
    let Some(rel) = wasi::rel_of(path) else {
        return Err(Unreadable {
            // alloc: the error of a path outside the package.
            path: path.to_owned(),
            // alloc: as above.
            error: format!("a module reads only its package, at {}", wasi::PACKAGE),
        });
    };
    let Some(root) = root else { return Ok(false) };
    let mut text = std::mem::take(real).into_os_string();
    text.clear();
    text.push(root);
    *real = PathBuf::from(text);
    real.push(rel);
    Ok(true)
}

impl Inputs for Now<'_> {
    type Error = Unreadable;
    fn entry(&mut self, path: &Path) -> Result<blaze_usercode::trace::Entry, Unreadable> {
        match real(self.root, path, self.real)? {
            true => self.machine.entry(self.real.as_path()),
            false => Ok(blaze_usercode::trace::Entry::Missing),
        }
    }

    fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, Unreadable> {
        match real(self.root, path, self.real)? {
            true => self.machine.file(self.real.as_path()),
            false => Ok(None),
        }
    }

    fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, Unreadable> {
        match real(self.root, path, self.real)? {
            true => self.machine.dir(self.real.as_path()),
            false => Ok(None),
        }
    }

    fn env(&mut self, name: &OsStr) -> Option<&OsStr> {
        let (answers, env) = (self.answers, self.env);
        if name.as_encoded_bytes().first() == Some(&0) {
            let name = name.to_str()?;
            if let Some(said) = answers.get(name) {
                return Some(OsStr::new(said));
            }
            if self.unanswered.get(name).is_none() {
                self.unanswered.ask(name);
            }
            return None;
        }
        let name = name.to_str()?;
        let at = env.binary_search_by(|it| it.0.cmp(name)).ok()?;
        Some(OsStr::new(env[at].1))
    }

    fn env_all(&mut self) -> &[(OsString, OsString)] {
        let env = self.env;
        let same = self.all.len() == env.len()
            && self.all.iter().zip(env).all(|(had, &(name, value))| had.0 == *name && had.1 == *value);
        if !same {
            self.all.clear();
            // alloc: the module's environment owned, as `Inputs::env_all` lends it: made again
            // only when it is another than the thread's last lookup's.
            self.all.extend(env.iter().map(|&(name, value)| (name.into(), value.into())));
        }
        &self.all[..]
    }

    fn names(&mut self, path: &Path, out: &mut Names<'_>) -> Result<bool, Unreadable> {
        match real(self.root, path, self.real)? {
            true => self.machine.names(self.real.as_path(), out),
            false => Ok(false),
        }
    }
}

/// What the store has of `run` for its inputs as they are now, by `look` of what
/// [`index::find`] found, which runs with the store's guard held. A pass that met a question
/// of a span with no answer yet is thrown away: the questions are asked of `db` with no guard
/// alive, since answering one can expand other macros on this thread, and it looks again.
/// Each pass that does not end answers at least one more of the finitely many questions the
/// store's shapes of `run` ask.
#[allow(clippy::too_many_arguments)]
fn find_answered<T>(
    store: &blaze_cas::Reader,
    run: &RunKey,
    root: Option<&Path>,
    env: &[(&str, &str)],
    shapes: &Shapes,
    db: Option<&Db>,
    room: &mut Room,
    look: impl Fn(Found<'_>) -> T,
) -> Result<T, FindError<Unreadable>> {
    let none = inputs::Env::default();
    let machine = inputs::Now { files: Files::shared(), env: &none };
    let Room { answers, unanswered, real, all } = room;
    answers.clear();
    loop {
        unanswered.clear();
        let mut now = Now {
            root,
            machine,
            env,
            answers: &*answers,
            unanswered: &mut *unanswered,
            real: &mut *real,
            all: &mut *all,
        };
        let looked = {
            let guard = blaze_cas::enter();
            index::find(store, &guard, run, &mut now).map(&look)
        };
        if unanswered.is_empty() {
            return looked;
        }
        for (read, _) in unanswered.iter() {
            answers.answer(read, shapes, db);
        }
    }
}

/// A buffer of this thread's, cleared, given back when dropped: a call's tokens are encoded
/// without allocating once the thread has encoded one as large.
struct Reused(Vec<u8>);

thread_local! {
    // alloc: none, an empty pool until the thread encodes a call.
    static BUFFERS: std::cell::RefCell<Vec<Vec<u8>>> = const { std::cell::RefCell::new(Vec::new()) };
}

impl Reused {
    fn take() -> Reused {
        Reused(BUFFERS.with_borrow_mut(Vec::pop).unwrap_or_default())
    }
}

impl std::ops::Deref for Reused {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for Reused {
    fn drop(&mut self) {
        let mut buffer = std::mem::take(&mut self.0);
        buffer.clear();
        // A thread that is ending has no pool left to give it to.
        _ = BUFFERS.try_with(|it| it.borrow_mut().push(buffer));
    }
}

/// An expansion's span parts and shapes, a kept output's parts and what its lookup reuses,
/// this thread's, cleared where used, given back when dropped: a hit allocates none of them
/// once the thread has expanded one as large. An expansion while another runs on the thread
/// makes its own.
#[derive(Default)]
struct Scratch {
    parts: Parts,
    shapes: Shapes,
    kept: Parts,
    /// Where a kept output off a 4-byte boundary is copied to be read.
    aligned: Vec<u8>,
    room: Room,
}

thread_local! {
    static SCRATCH: std::cell::Cell<Scratch> = std::cell::Cell::new(Scratch::default());
}

struct Scratched(Scratch);

impl Scratched {
    fn take() -> Scratched {
        let mut it = SCRATCH.take();
        it.parts.clear();
        Scratched(it)
    }
}

impl Drop for Scratched {
    fn drop(&mut self) {
        let it = std::mem::take(&mut self.0);
        // A thread that is ending has no scratch left to give it to.
        _ = SCRATCH.try_with(|cell| cell.set(it));
    }
}

/// Expands the macro at `index` of the module whose expansions are kept under `code` in a
/// fresh instance: what is kept of it when the store has it, else by running it in what
/// `wasm` gives, which is asked for only then, and keeps what it made by what it read
/// ([`what_it_read`]) unless it read what can't be traced or left state behind, where
/// `keeping` says ([`blaze_expand::keeping`]). Nothing is kept where blaze-expand was not
/// installed (a bench).
#[allow(clippy::too_many_arguments)]
fn expand<'a>(
    keeping: Option<blaze_expand::Keeping>,
    code: &ContentId,
    wasm: &impl Fn() -> Result<&'a Wasm, String>,
    index: u32,
    listed: &Listed,
    db: Option<&Db>,
    call: &Call<'_>,
) -> Ran {
    let start = Instant::now();
    let failed = |why: String| {
        // alloc: the message of an expansion that failed.
        ProcMacroExpansionError::System(format!("the wasm proc macro `{}`: {why}", listed.name))
    };
    let (env, root) = guest_env(call.env);
    let mut scratch = Scratched::take();
    let Scratch { parts, shapes, kept, aligned, room } = &mut scratch.0;
    let sites = [call.def_site, call.call_site, call.mixed_site].map(|span| parts.pack(span));
    let mut input = Reused::take();
    let mut attr = Reused::take();
    wire::relative(call.subtree, parts, &mut input.0);
    // rust-analyzer gives an attribute written without arguments (`#[twice]`) none, which
    // the module is given as no bytes, an empty stream.
    let attr = match (call.attrs, listed.kind) {
        (Some(it), _) => {
            wire::relative(it, parts, &mut attr.0);
            Some(attr)
        }
        (None, ProcMacroKind::Attr) => Some(attr),
        (None, _) => None,
    };
    shapes.fill(parts.all(), sites);
    let shapes = &*shapes;
    let mut given = Given::new(RunKind::ProcMacro);
    given
        .part("host", HOST_VERSION.as_bytes())
        .part("name", listed.name.as_bytes())
        .part("kind", &[listed.kind as u8])
        .part("input", &input)
        .part("attrs", attr.as_deref().unwrap_or(b"-"))
        .part("spans", &shapes.bytes);
    given.env_keyed(env.iter().map(|(name, value)| (name.as_bytes(), value.as_bytes())));
    let run = RunKey { code: *code, given: given.finish() };
    let restore = |tree: tt::TopSubtree| match root {
        Some(root) => placed(tree, root),
        None => tree,
    };
    let kept = std::cell::Cell::from_mut(kept);
    let aligned = std::cell::Cell::from_mut(aligned);
    let found = match keeping {
        Some(keeping) => {
            let found = find_answered(
                &keeping.expansions.store,
                &run,
                root,
                &env,
                shapes,
                db,
                room,
                |found| match found {
                    // Read in place, under the guard: a hit copies nothing out of the store.
                    Found::Hit { value, .. } if keeping.mode == Mode::Keep => {
                        let mut parts = kept.take();
                        let mut copy = aligned.take();
                        let read = shapes.read(value, &mut parts, &mut copy);
                        aligned.set(copy);
                        kept.set(parts);
                        Err(read)
                    }
                    Found::Hit { value, .. } => Ok(Some(blaze_expand::checked(value))),
                    Found::Miss { .. } => Ok(None),
                },
            );
            match found {
                Ok(Ok(found)) => found,
                Ok(Err(kept)) => {
                    let output = kept.map(&restore).map_err(|why| {
                        // alloc: the message of a kept output refused.
                        failed(format!("the store's expansion of it is not one: {why}"))
                    });
                    count(Counted::Found, start.elapsed());
                    return Ran { output, found: true, retained: false, took: start.elapsed() };
                }
                Err(error) => {
                    // alloc: the message of a store that can't be read.
                    let why = match error {
                        // alloc: the message of a store that can't be read.
                        FindError::Corrupt(corrupt) => corrupt.to_string(),
                        FindError::NotAShape { key, why } => {
                            // alloc: the message of a store that can't be read.
                            format!("the store's {key:?} is not a shape: {why}")
                        }
                        // alloc: the message of a store that can't be read.
                        FindError::Inputs(unreadable) => unreadable.to_string(),
                    };
                    let took = start.elapsed();
                    return Ran { output: Err(failed(why)), found: false, retained: false, took };
                }
            }
        }
        None => None,
    };
    let ran = wasm().map_err(ProcMacroExpansionError::System).and_then(|wasm| {
        let mut state = State::empty();
        // clone: the instance's own copy, made only when the macro runs.
        state.anchors = shapes.anchors.clone();
        // clone: the instance's own copy, made only when the macro runs.
        state.contexts = shapes.contexts.clone();
        state.parts = std::mem::take(parts);
        state.sites = sites;
        state.files = wasi::Files::new(root);
        state.environ = Environ::of(&env);
        // Its output is placed by `restore`, not by the instance.
        let mut instance = wasm.instance_with(state, None)?;
        instance.run(index, db, &input, attr.as_deref())?;
        Ok(instance.store.into_data())
    });
    let mut retained = false;
    let output = ran.and_then(|mut state| {
        let tree = wire::read(&state.output, &state.parts)
            // alloc: the message of an output refused.
            .map_err(|e| ProcMacroExpansionError::System(format!("`{}`: {e}", listed.name)))?;
        retained = state.retained;
        let value = shapes.written(&state.parts, &state.output);
        std::mem::swap(parts, &mut state.parts);
        if let Some(kept) = &found {
            if value.as_deref().map(blaze_expand::checked).as_ref() != Some(kept) {
                // alloc: the message of an expansion that failed.
                return Err(failed(format!(
                    "{}=check: it expanded otherwise than the store kept it, under a key of \
                     every input it read",
                    blaze_expand::CACHE
                )));
            }
            return Ok(restore(tree));
        }
        if keeping.is_none() {
            return Ok(restore(tree));
        }
        let traced = what_it_read(&mut state);
        match (traced, value) {
            (None, _) => count(Counted::Asked, Duration::ZERO),
            (Some(traced), _) if index::untraceable(&traced) => {
                count(Counted::Untraceable, Duration::ZERO)
            }
            (Some(_), _) if state.retained => {}
            (Some(traced), Some(value)) => {
                // alloc: what names an output the store keeps, made only when the macro ran.
                let what = format!("the wasm proc macro `{}`", listed.name);
                blaze_expand::put(Put { run, traced, value, what });
                count(Counted::Kept, Duration::ZERO);
            }
            (Some(_), None) => {}
        }
        Ok(restore(tree))
    });
    count(Counted::Ran, start.elapsed());
    Ran { output, found: false, retained, took: start.elapsed() }
}

/// The key a module's [`Verdict`] is kept under in the store, by the module's digest, for every
/// process and machine that loads the same module. A stateful module is one an expansion of
/// left what it allocated live, for a later one to find.
fn verdict_key(digest: &[u8; 32]) -> [u8; 32] {
    let mut key = keyed::Key::new("whether a proc-macro module keeps state 1");
    key.field(digest);
    key.done()
}

fn verdict(digest: &[u8; 32]) -> Verdict {
    match store().and_then(|it| it.get_lasting(&verdict_key(digest))).as_deref() {
        Some(STATELESS) => Verdict::Stateless,
        Some(STATEFUL) => Verdict::Stateful,
        _ => Verdict::Unseen,
    }
}

/// The modules loaded in this process, for [`flush`] to tell the store what it saw of them.
// alloc: none, an empty list until a module loads.
static MODULES: Mutex<Vec<std::sync::Weak<Lazy>>> = Mutex::new(Vec::new());

/// A macro of a module, as rust-analyzer expands it.
pub struct Expander {
    wasm: AssertUnwindSafe<Arc<Lazy>>,
    index: u32,
    listed: Listed,
}

/// A module compiled when it first expands, by when the store can give its machine code.
struct Lazy {
    path: PathBuf,
    known: Known,
    compiled: OnceLock<Result<&'static Wasm, String>>,
    /// What the store said of it when it was loaded, which decides for as long as it is:
    /// whether a macro needs a session is asked of the database's queries, which must not
    /// see it change. And what its expansions in this process did since.
    seen: Seen,
    /// Each crate's seat in each database, found with no lock by the hash of the two
    /// ([`seat_id`]): crates expand the library at once, and a crate's expansions take turns.
    seats: Sharded<Seat>,
    /// Every seat, for [`Lazy::sweep`]; locked only when a seat is made and when a database
    /// moves to a new revision.
    made: Mutex<Vec<Idx<Seat>>>,
    /// The revision each database's seats were last swept for.
    swept: Mutex<FxHashMap<Nonce, salsa::Revision>>,
}

/// The session of one crate in one database, and the turns its expansions take in it: a module
/// that keeps state between calls is a function of the calls before, so one runs at a time,
/// in the order they came. No lock is held while one runs: it takes the session out of the
/// queue, so the database it asks can expand any other macro meanwhile.
struct Seat<Of = (Nonce, Crate)> {
    of: Of,
    /// Locked only to take a turn or end one, never while an expansion runs.
    queue: Mutex<Queue>,
    turn: Condvar,
}

#[derive(Default)]
struct Queue {
    /// The session and the revision it is of, while no expansion runs in it; `None` before
    /// the first.
    session: Option<(salsa::Revision, Result<Session, String>)>,
    /// The thread an expansion runs in it on, while one does.
    running: Option<ThreadId>,
    /// The next ticket handed out, and the one whose turn it is.
    next: u64,
    serving: u64,
}

/// One expansion's turn in a [`Seat`]: the session is its until the turn is dropped.
struct Turn<'a, Of> {
    seat: &'a Seat<Of>,
    session: Option<(salsa::Revision, Result<Session, String>)>,
}

impl<Of> Seat<Of> {
    fn new(of: Of) -> Seat<Of> {
        Seat { of, queue: Mutex::new(Queue::default()), turn: Condvar::new() }
    }

    /// This thread's turn, once every expansion that came before has had its. An expansion
    /// asked for in the seat while this thread's own runs in it (a question of a span that
    /// expands the crate's macros again) is refused: the module's instance is in the middle
    /// of a call, and the expansion can't wait for itself.
    fn take(&self) -> Result<Turn<'_, Of>, ()> {
        let me = std::thread::current().id();
        let mut queue = self.queue.lock().unwrap_or_else(|it| it.into_inner());
        if queue.running == Some(me) {
            return Err(());
        }
        let ticket = queue.next;
        queue.next += 1;
        while queue.serving != ticket {
            queue = self.turn.wait(queue).unwrap_or_else(|it| it.into_inner());
        }
        queue.running = Some(me);
        let session = queue.session.take();
        Ok(Turn { seat: self, session })
    }
}

impl<Of> Drop for Turn<'_, Of> {
    fn drop(&mut self) {
        let mut queue = self.seat.queue.lock().unwrap_or_else(|it| it.into_inner());
        queue.session = self.session.take();
        queue.running = None;
        queue.serving += 1;
        drop(queue);
        self.seat.turn.notify_all();
    }
}

/// Feeds what a value hashes into to a content ID, so two values that hash differently have
/// different IDs.
struct Feed(Hasher);

impl std::hash::Hasher for Feed {
    fn write(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    fn finish(&self) -> u64 {
        unreachable!("a feed is finished as a content ID")
    }
}

/// What a seat is found by: its database and crate.
fn seat_id(of: &(Nonce, Crate)) -> ContentId {
    let mut feed = Feed(Hasher::new("blaze v2 wasm macro seat 1"));
    std::hash::Hash::hash(of, &mut feed);
    feed.0.finish()
}

impl Lazy {
    fn new(path: &Path, known: Known) -> Lazy {
        Lazy {
            // alloc: setup, once a module: its path, compiled from when it first expands.
            path: path.to_owned(),
            known,
            compiled: OnceLock::new(),
            seen: Seen::new(verdict(&known.digest)),
            seats: Sharded::new(),
            // alloc: none, an empty list until a crate expands in a session.
            made: Mutex::new(Vec::new()),
            swept: Mutex::new(FxHashMap::default()),
        }
    }

    /// The seat of `of`, made the first time it is asked for.
    fn seat(&self, of: (Nonce, Crate)) -> &Seat {
        let id = seat_id(&of);
        let at = match self.seats.find(&id) {
            Some(at) => at,
            None => {
                // Also true when another thread interned it meanwhile: the seat is then listed
                // twice, which a sweep doesn't mind.
                let mut new = false;
                let at = self.seats.intern(id, || {
                    new = true;
                    Seat::new(of)
                });
                if new {
                    self.made.lock().unwrap_or_else(|it| it.into_inner()).push(at);
                }
                at
            }
        };
        let seat = self.seats.get(at);
        assert_eq!(seat.of, of, "two seats with one content ID");
        seat
    }

    /// Drops the sessions of `nonce`'s database of revisions before `revision`, which it has
    /// moved to: once per revision, by the first expansion that found its seat's session old.
    /// A session in use is the current revision's, as salsa runs one revision at a time.
    fn sweep(&self, nonce: Nonce, revision: salsa::Revision) {
        {
            let mut swept = self.swept.lock().unwrap_or_else(|it| it.into_inner());
            if swept.get(&nonce) == Some(&revision) {
                return;
            }
            swept.insert(nonce, revision);
        }
        // clone: once a revision a database moves to: the seats, read with no lock held.
        let made: Vec<Idx<Seat>> = self.made.lock().unwrap_or_else(|it| it.into_inner()).clone();
        // alloc: as above: the sessions to drop, once a revision.
        let mut old = Vec::new();
        for at in made {
            let seat = self.seats.get(at);
            if seat.of.0 != nonce {
                continue;
            }
            let mut queue = seat.queue.lock().unwrap_or_else(|it| it.into_inner());
            if queue.session.as_ref().is_some_and(|(of, _)| *of != revision) {
                old.extend(queue.session.take());
            }
        }
        // Dropped with no seat locked.
        drop(old);
    }

    fn get(&self) -> Result<&'static Wasm, String> {
        let compiled = self.compiled.get_or_init(|| {
            Wasm::known(&self.path, self.known, None, store(), self.kind())
        });
        let wasm = *compiled.as_ref().map_err(Clone::clone)?;
        wasm.keep_code(store());
        Ok(wasm)
    }

    /// Whether its macros expand in a session: unless the store says none of its expansions
    /// left state.
    fn stateful(&self) -> bool {
        self.seen.verdict() != Verdict::Stateless
    }

    /// What its instances are: fresh ones, pooled, unless its macros expand in a session.
    fn kind(&self) -> Kind {
        Kind::of(self.seen.verdict())
    }

    /// Notes what an expansion that ran did. One that left state is told the store at once;
    /// that none did, by [`flush`], once a build has run them.
    fn saw(&self, ran: &Ran) {
        if ran.found || ran.output.is_err() {
            return;
        }
        let Some(contradicted) = self.seen.saw(ran.retained) else { return };
        if let Some(store) = store() {
            store.insert_lasting(&verdict_key(&self.known.digest), STATEFUL);
            store.flush();
        }
        if contradicted {
            eprintln!(
                "blaze: the proc macros of {} keep state between expansions, which the store \
                 said they did not: they expand in a session from the next load",
                self.path.display()
            );
        }
    }
}

/// The file beside a module that has its list of macros, for the module whose digest is on
/// its first line, with its source identity and the [`stamp`] of the files it
/// was written for on the second: a load reads it there and compiles nothing.
fn list_file(path: &Path) -> PathBuf {
    // alloc: setup, once a module: the name of the file beside it.
    let mut named = path.as_os_str().to_owned();
    named.push(".macros");
    PathBuf::from(named)
}

impl fmt::Debug for Expander {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Expander").field("name", &self.listed.name).finish_non_exhaustive()
    }
}

/// What is counted of macro expansions and loaded modules for the daemon's build statistics,
/// each with the time it took, on the thread that did it ([`blaze_expand::tally`]).
#[derive(Clone, Copy)]
enum Counted {
    /// Expansions run in a module, from their call.
    Ran,
    /// Expansions the store had, from their call.
    Found,
    /// Expansions left for the store's owner to keep ([`blaze_expand::take_puts`]).
    Kept,
    /// Expansions run that were not kept, as they asked the database what their key does not
    /// say or met a file they could not read.
    Asked,
    /// Expansions run that were not kept, as they read the clock or randomness.
    Untraceable,
    /// Modules made ready to run in this process (machine code loaded or compiled, linked).
    Loaded,
}

const COUNTED: usize = Counted::Loaded as usize + 1;

static TALLY: Tally<COUNTED> = Tally::new();

thread_local! {
    static MINE: Mine<COUNTED> = Mine::of(&TALLY);
}

fn count(what: Counted, took: Duration) {
    // A thread that is ending has no block left to count into.
    _ = MINE.try_with(|it| it.add(what as usize, took));
}

/// The expansions so far, for the daemon's log: `None` before the first.
pub fn report() -> Option<String> {
    let counted = |what: Counted| TALLY.get(what as usize);
    let (ran, running) = counted(Counted::Ran);
    let (found, finding) = counted(Counted::Found);
    if ran + found == 0 {
        return None;
    }
    let (loaded, loading) = counted(Counted::Loaded);
    // alloc: the edge: the daemon's log.
    let mut said = format!(
        "wasm proc macros: {ran} ran in {running:.3}s, {found} from the store in \
         {finding:.3}s, {loaded} modules loaded in {loading:.3}s"
    );
    let rest = [
        (Counted::Kept, "kept"),
        (Counted::Asked, "not kept, as they asked the database what their key does not say or met a file they could not read"),
        (Counted::Untraceable, "not kept, as they read the clock or randomness"),
    ];
    for (which, what) in rest {
        let (calls, _) = counted(which);
        if calls != 0 {
            _ = write!(said, "; {calls} {what}");
        }
    }
    let modules = MODULES.lock().unwrap_or_else(|e| e.into_inner());
    let sessions = modules.iter().filter_map(std::sync::Weak::upgrade).filter(|it| it.stateful());
    let sessions = sessions.count();
    if sessions != 0 {
        _ = write!(said, "; {sessions} modules expand in sessions (keep state or not seen yet)");
    }
    Some(said)
}

/// Writes out that a module whose expansions ran and none of which left state is stateless,
/// for the next process to find: the store holds what is added in memory until it is flushed.
pub fn flush() {
    let Some(store) = store() else { return };
    let mut told = false;
    let mut modules = MODULES.lock().unwrap_or_else(|e| e.into_inner());
    modules.retain(|it| it.strong_count() != 0);
    for lazy in modules.iter().filter_map(std::sync::Weak::upgrade) {
        // Another process that saw one leave state may have told the store since the load.
        if lazy.seen.flush() && verdict(&lazy.known.digest) == Verdict::Unseen {
            store.insert_lasting(&verdict_key(&lazy.known.digest), STATELESS);
            told = true;
        }
    }
    if told {
        store.flush();
    }
}

/// The store of modules' machine code and of what was seen of their state, once the daemon has
/// told it the toolchain. Expansions are kept in blaze-expand's ([`blaze_expand::keeping`]).
fn store() -> Option<&'static blaze_store::Store> {
    blaze_store::toolchain()?;
    blaze_store::Store::global()
}

impl ProcMacroExpander for Expander {
    fn requires_session(&self) -> bool {
        self.wasm.stateful()
    }

    fn expand(
        &self,
        db: &Db,
        ProcMacroCall { subtree, attrs, env, def_site, call_site, mixed_site, .. }: ProcMacroCall<'_>,
    ) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        if self.wasm.stateful() {
            return Err(ProcMacroExpansionError::System(
                // alloc: the message of a call refused.
                "a proc macro that keeps state expands only for a calling crate".to_owned(),
            ));
        }
        let call = Call { subtree, attrs, env, def_site, call_site, mixed_site };
        // What its expansions are kept under is known without the module: then one the store
        // has neither loads nor compiles it.
        let wasm = || self.wasm.get();
        let code = code(&self.wasm.known.digest);
        let keeping = blaze_expand::keeping();
        let ran = expand(keeping, &code, &wasm, self.index, &self.listed, Some(db), &call);
        self.wasm.saw(&ran);
        ran.output
    }

    fn expand_in(
        &self,
        db: &Db,
        calling_crate: base_db::Crate,
        call: ProcMacroCall<'_>,
    ) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        if !self.wasm.stateful() {
            return self.expand(db, call);
        }
        let ProcMacroCall { subtree, attrs, env, def_site, call_site, mixed_site, .. } = call;
        // A new revision must rerun unchanged registrations before changed uses.
        db.report_untracked_read();
        let (nonce, revision) = db.nonce_and_revision();
        let wasm = self.wasm.get().map_err(ProcMacroExpansionError::System)?;
        let seat = self.wasm.seat((nonce, calling_crate));
        let mut turn = seat.take().map_err(|()| {
            // alloc: the message of a call refused.
            ProcMacroExpansionError::System(format!(
                "`{}` was asked to expand while one of its crate's expansions in the same \
                 module runs on this thread (a question it asked expands it again); the module \
                 keeps state between calls, so the two can't run at once",
                self.listed.name
            ))
        })?;
        if turn.session.as_ref().is_some_and(|(of, _)| *of != revision) {
            turn.session = None;
            self.wasm.sweep(nonce, revision);
        }
        let (_, session) = turn.session.get_or_insert_with(|| {
            let session = wasm.session(env).map_err(|e| match e {
                ProcMacroExpansionError::Panic(why) | ProcMacroExpansionError::System(why) => why,
            });
            (revision, session)
        });
        // clone: the message of a session that could not start, given each call.
        let session = session.as_mut().map_err(|e| ProcMacroExpansionError::System(e.clone()))?;
        let call = Call { subtree, attrs, env, def_site, call_site, mixed_site };
        let ran = session.expand(self.index, &self.listed, Some(db), &call);
        self.wasm.saw(&ran);
        ran.output
    }

    // dyn: the edge: hir-expand's trait compares the expanders it holds so, once a macro.
    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool {
        // dyn: as above.
        (other as &dyn Any).downcast_ref::<Expander>().is_some_and(|other| {
            self.wasm.known.digest == other.wasm.known.digest && self.index == other.index
        })
    }
}

/// The tokens of `text`, each at `span`'s anchor and context over its range of the text, as
/// rust-analyzer's proc-macro server lexes a string: a macro's input for a test or a bench.
pub fn tokens(text: &str, span: Span) -> Result<tt::TopSubtree, String> {
    Ok(wire::top_subtree(lex::stream(text, span)?, span))
}

/// The macros of the module at `path`, for rust-analyzer: what blaze registers with
/// [`load_cargo::wasm_macros_with`].
pub fn load(path: &AbsPath) -> ProcMacroLoadResult {
    let failed = |e: String| {
        base_db::ProcMacroLoadingError::ProcMacroSrvError(
            // alloc: the message of a module that can't be loaded.
            format!("proc macros compiled to WebAssembly: {e}").into(),
        )
    };
    let path: &Path = path.as_ref();
    let (lazy, text) = match listing(path) {
        Some((known, text)) => (Lazy::new(path, known), text),
        None => {
            // alloc: setup, once a module: its bytes, or the message of a file that can't be read.
            let bytes = std::fs::read(path).map_err(|e| failed(format!("{}: {e}", path.display())))?;
            let digest = keyed::sha256(&bytes);
            let known = Known { digest, identity: None };
            let wasm = Wasm::known(path, known, Some(&bytes), store(), kind_for(&digest))
                .map_err(failed)?;
            let text = wasm.list().map_err(failed)?;
            list(path, wasm, &text);
            let known = Known { digest: wasm.digest, identity: Some(wasm.identity) };
            let lazy = Lazy::new(path, known);
            // The load's own verdict, so the kind it was compiled for, unless the store was
            // told otherwise meanwhile: then `get` compiles for the other.
            if lazy.kind() == wasm.kind {
                _ = lazy.compiled.set(Ok(wasm));
            }
            (lazy, text)
        }
    };
    // alloc: setup, once a module: what its expanders share.
    let lazy = Arc::new(lazy);
    MODULES.lock().unwrap_or_else(|e| e.into_inner()).push(Arc::downgrade(&lazy));
    let macros = listed(&text).into_iter().enumerate().map(|(index, listed)| ProcMacro {
        name: Symbol::intern(&listed.name),
        kind: listed.kind,
        // alloc: the edge, once a macro: hir-expand holds each expander in an `Arc`.
        expander: Arc::new(Expander {
            // clone: setup, once a macro: its share of the module.
            wasm: AssertUnwindSafe(lazy.clone()),
            index: index as u32,
            listed,
        }),
        disabled: false,
    });
    // alloc: setup, once a module: the list rust-analyzer is given.
    Ok(macros.collect())
}

#[cfg(test)]
mod tests {
    use blaze_core::Untraceable;
    use span::{EditionedFileId, ErasedFileAstId};

    use super::*;

    fn span(start: u32, end: u32) -> Span {
        let file_id = EditionedFileId::new(span::FileId::from_raw(7), span::Edition::CURRENT);
        Span {
            range: range(start, end),
            anchor: SpanAnchor { file_id, ast_id: span::ROOT_ERASED_FILE_AST_ID },
            ctx: SyntaxContext::root(span::Edition::CURRENT),
        }
    }

    fn scratch(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("blaze-macro-wasm-{name}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_guest_is_its_own_text_then_the_bridges_api() {
        let api = include_str!("../../blaze-expand/src/native/proc_macro/api.rs");
        assert!(GUEST.ends_with(api));
        assert_eq!(GUEST.matches("\nfn taken(").count(), 1);
        assert!(GUEST[..GUEST.len() - api.len()].ends_with('\n'), "the API starts a line");
    }

    #[test]
    fn a_stream_comes_back_as_it_went() {
        let text =
            "#[derive(Debug)] struct S<T> { a: [u8; 3], b: (T, \"s\", b'x', r#\"raw\"#, 2.5e3) }";
        let top = tokens(text, span(3, 9)).unwrap();
        let sites = [span(0, 1), span(1, 2), span(2, 3)];
        let mut parts = Parts::default();
        for site in sites {
            parts.pack(site);
        }
        let mut bytes = Vec::new();
        wire::relative(&top, &mut parts, &mut bytes);
        let back = decode(&bytes, sites).unwrap();
        assert!(back == top, "{back} came back of {top}");
    }

    #[test]
    fn a_tree_is_decoded_whole_and_of_the_expansions_parts_or_not_at_all() {
        let sites = [span(0, 1), span(1, 2), span(2, 3)];
        let mut parts = Parts::default();
        for site in sites {
            parts.pack(site);
        }
        let mut bytes = Vec::new();
        wire::relative(&tokens("a (b [c]) \"d\" 1u8", span(4, 5)).unwrap(), &mut parts, &mut bytes);
        assert!(decode(&bytes, sites).is_ok());
        assert!(decode(&u32::MAX.to_le_bytes(), sites).is_err());
        for len in 0..bytes.len() {
            assert!(decode(&bytes[..len], sites).is_err(), "{len} of {} bytes", bytes.len());
        }
        let mut longer = bytes.clone();
        longer.extend([0; 4]);
        assert!(decode(&longer, sites).unwrap_err().contains("bytes after the token tree"));
        let at = wire::sections(&bytes).unwrap().parts;
        let mut past = bytes.clone();
        past[at..at + 4].copy_from_slice(&3u32.to_le_bytes());
        assert!(decode(&past, sites).unwrap_err().contains("span parts"));
        let mut raw = bytes.clone();
        raw[at + 4..at + 8].copy_from_slice(&1u32.to_le_bytes());
        assert!(decode(&raw, sites).unwrap_err().contains("span parts"));
    }

    /// What the bridge's host is given to ask, which a span function it answers alone never
    /// asks.
    struct Unasked;

    impl native_proc_macro::__native::Ask for Unasked {
        fn join(&mut self, _: Span, _: Span) -> Option<Span> {
            panic!("`join` was asked")
        }
        fn parent(&mut self, _: Span) -> Option<Span> {
            panic!("`parent` was asked")
        }
        fn source(&mut self, _: Span) -> Option<Span> {
            panic!("`source` was asked")
        }
        fn subspan(&mut self, _: Span, _: u64, _: u64) -> Option<Span> {
            panic!("`subspan` was asked")
        }
        fn byte_range(&mut self, _: Span) -> [u32; 2] {
            panic!("`byte_range` was asked")
        }
        fn line_column(&mut self, _: Span) -> Option<(u32, u32)> {
            panic!("`line_column` was asked")
        }
        fn file(&mut self, _: Span) -> Option<String> {
            panic!("`file` was asked")
        }
        fn local_file(&mut self, _: Span) -> Option<String> {
            panic!("`local_file` was asked")
        }
        fn source_text(&mut self, _: Span) -> Option<String> {
            panic!("`source_text` was asked")
        }
        fn ts_from_str(&mut self, _: &mut Parts, _: &str, _: Span) -> Result<Vec<u8>, String> {
            panic!("`ts_from_str` was asked")
        }
        fn literal_from_str(&mut self, _: &mut Parts, _: &str, _: Span) -> Option<Vec<u8>> {
            panic!("`literal_from_str` was asked")
        }
    }

    /// `resolved_at`, `located_at`, `start` and `end`, which the bridge's `Host` answers of its
    /// parts without asking, give the word this host's function of the same name gives, for
    /// every pair of words of one expansion: narrow, wide, of a fixup, of another context or
    /// anchor, and words it never gave out.
    #[test]
    fn a_span_function_the_bridge_answers_alone_gives_the_wasm_hosts_word() {
        let mut linker = Linker::new(engine());
        add_host(&mut linker).unwrap();
        let mut store = Store::new(engine(), State::empty());
        let func = |store: &mut Store<State>, name: &str| {
            linker.get(&mut *store, HOST, name).unwrap().into_func().expect("a function")
        };
        let resolved_at =
            func(&mut store, "span_resolved_at").typed::<(u64, u64), u64>(&store).unwrap();
        let located_at =
            func(&mut store, "span_located_at").typed::<(u64, u64), u64>(&store).unwrap();
        let start = func(&mut store, "span_start").typed::<u64, u64>(&store).unwrap();
        let end = func(&mut store, "span_end").typed::<u64, u64>(&store).unwrap();

        let file_id = EditionedFileId::new(span::FileId::from_raw(7), span::Edition::CURRENT);
        let other = SyntaxContext::root(span::Edition::Edition2015);
        let anchored = |ast_id| SpanAnchor { file_id, ast_id };
        let spans = [
            span(0, 1),
            span(1, 2),
            span(2, 3),
            span(10, 20),
            Span { ctx: other, ..span(4, 9) },
            Span { anchor: anchored(ErasedFileAstId::from_raw(3)), ..span(30, 31) },
            Span { anchor: anchored(FIXUP_ERASED_FILE_AST_ID_MARKER), ..span(5, 8) },
            span(0x100, 0x100 + 0x2_0000),
        ];
        let mut parts = Parts::default();
        // alloc: a test's words.
        let mut words = Vec::new();
        for it in spans {
            let word = parts.pack(it);
            assert_eq!(store.data_mut().parts.pack(it), word);
            words.push(word);
        }
        // No span at all, and a narrow word of a part the expansion has not.
        words.extend([NONE, 5 | u64::from(3u32 | 40 << 16) << 32]);
        let sites = [words[0], words[1], words[2]];
        store.data_mut().sites = sites;

        let mut ask = Unasked;
        let mut host = native_proc_macro::__native::Host { parts: &mut parts, sites, ask: &mut ask };
        for &a in &words {
            assert_eq!(host.span_start(a), start.call(&mut store, a).unwrap(), "start of {a:#x}");
            assert_eq!(host.span_end(a), end.call(&mut store, a).unwrap(), "end of {a:#x}");
            for &b in &words {
                let wasm = resolved_at.call(&mut store, (a, b)).unwrap();
                assert_eq!(host.span_resolved_at(a, b), wasm, "{a:#x} resolved at {b:#x}");
                let wasm = located_at.call(&mut store, (a, b)).unwrap();
                assert_eq!(host.span_located_at(a, b), wasm, "{a:#x} located at {b:#x}");
            }
        }
        assert!(host.parts.all() == store.data().parts.all(), "the two made the same parts");
        let moved = host.span_resolved_at(words[0], words[4]);
        let moved = host.get(moved).unwrap();
        assert_eq!((moved.range, moved.ctx), (spans[0].range, other));
        let fixup = words[6];
        assert_eq!(host.span_start(fixup), fixup);
        let ended = host.span_end(words[3]);
        let ended = host.get(ended).unwrap();
        assert_eq!(ended.range, TextRange::empty(spans[3].range.end()));
    }

    #[test]
    fn a_module_that_is_not_one_is_refused_unrun() {
        assert!(check_module(b"\0asm\x01\0\0\0").is_ok());
        assert!(check_module(b"not a module").is_err());
        assert_eq!(carried_list(b"not a module"), None);
    }

    #[test]
    fn a_module_carries_its_list_in_a_custom_section() {
        let custom = |name: &str, payload: &[u8]| {
            let mut body = vec![name.len() as u8];
            body.extend_from_slice(name.as_bytes());
            body.extend_from_slice(payload);
            let mut section = vec![0, body.len() as u8];
            section.extend(body);
            section
        };
        let mut module = b"\0asm\x01\0\0\0".to_vec();
        // A type section of no types, then a custom section of another name.
        module.extend_from_slice(&[1, 1, 0]);
        module.extend(custom("name", b"\0\0"));
        assert_eq!(carried_list(&module), None);
        module.extend(custom(exports::LIST_SECTION, b"0\tSerialize\tserde\n"));
        assert_eq!(carried_list(&module).as_deref(), Some("0\tSerialize\tserde\n"));
        module.truncate(module.len() - 1);
        assert_eq!(carried_list(&module), None, "a section that runs past the end");
    }

    #[test]
    fn a_prepared_module_is_loaded_without_compiling_or_reading_it() {
        let dir = scratch("prepared");
        blaze_store::compiles_against("blaze-macro-wasm tests");
        let store: &'static blaze_store::Store =
            Box::leak(Box::new(blaze_store::Store::open_at(&dir.join("store")).unwrap()));
        let path = dir.join("m.wasm");
        let list = "0\tSerialize\tserde\n";
        let mut module = b"\0asm\x01\0\0\0".to_vec();
        let name = exports::LIST_SECTION.as_bytes();
        module.extend_from_slice(&[0, (1 + name.len() + list.len()) as u8, name.len() as u8]);
        module.extend_from_slice(name);
        module.extend_from_slice(list.as_bytes());
        std::fs::write(&path, &module).unwrap();
        prepare_soon(&path, Some(store)).unwrap();
        let (known, text) = listing(&path).expect("the list is written before the compile ends");
        assert_eq!(text, list);
        assert_eq!(known.digest, keyed::sha256(&module));
        // Waits for the compile `prepare_soon` started, which keeps the code in the store, under
        // a key of no path, and nowhere beside the module.
        let kind = kind_for(&known.digest);
        Wasm::known(&path, known, None, Some(store), kind).unwrap();
        let key = code_key(&known.digest, engine_of(kind));
        assert!(store.has_lasting(&key));
        let beside: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|it| it.unwrap().file_name())
            .filter(|it| it.to_string_lossy().ends_with(".cwasm"))
            .collect();
        assert!(beside.is_empty(), "{beside:?}");
        store.seal();
        assert!(store.get_lasting(&key).is_some());
        // Another write of the module, even of the same bytes, is listed again.
        std::fs::write(&path, &module).unwrap();
        let later = std::time::SystemTime::now() + Duration::from_secs(5);
        std::fs::File::options().write(true).open(&path).unwrap().set_modified(later).unwrap();
        assert!(listing(&path).is_none());
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_question_of_a_span_is_asked_again_of_the_span_in_its_place() {
        let asked = span(3, 9);
        let of = |span: Span| Shapes::of(&[(span.anchor, span.ctx)], [NONE; 3]);
        let read = read_of(Value::LineColumn, [3, 9, 0, 0]);
        assert_eq!(read, "\0l3,9,0,0", "the question as the store's read sets have it");
        let (value, again) = Value::parse(&read, &of(asked)).unwrap();
        assert!(value == Value::LineColumn);
        assert_eq!(again, asked);
        // The next call's span in the same place, of another anchor.
        let mut moved = asked;
        moved.anchor.ast_id = ErasedFileAstId::from_raw(5);
        assert_eq!(Value::parse(&read, &of(moved)).unwrap().1, moved);
        assert!(Value::parse(&read_of(Value::File, [0, 0, 1, 0]), &of(asked)).is_none());
        assert!(Value::parse("src/lib.rs", &of(asked)).is_none());
        let answer = SubResponse::LineColumnResult { line: 3, column: 4 };
        assert_eq!(said_of(Value::LineColumn, Some(&answer)), "3:4");
        assert_eq!(said_of(Value::SourceText, None), "-");
        let range = SubResponse::ByteRangeResult { range: 2..7 };
        assert_eq!(said_of(Value::ByteRange, Some(&range)), "2..7");
        let file = SubResponse::FilePathResult { name: "src/lib.rs".to_owned() };
        let mut digest = String::new();
        blaze_format::hex::encode_into(&keyed::digest_bytes(b"src/lib.rs"), &mut digest);
        assert_eq!(said_of(Value::File, Some(&file)), digest);
    }

    fn read_of(value: Value, fields: [u32; 4]) -> String {
        let mut out = String::new();
        value.read(fields, &mut out);
        out
    }

    fn said_of(value: Value, answer: Option<&SubResponse>) -> String {
        let mut out = String::new();
        value.said(answer, &mut out);
        out
    }

    #[test]
    fn a_module_reads_its_package_and_nothing_else() {
        let dir = scratch("files");
        std::fs::create_dir_all(dir.join("package/src")).unwrap();
        std::fs::write(dir.join("package/src/a.txt"), "a").unwrap();
        std::fs::write(dir.join("outside.txt"), "o").unwrap();
        let files = wasi::Files::new(Some(&dir.join("package")));
        let resolve = |rel: &str, path: &str| files_resolve(&files, rel, path);
        assert_eq!(resolve("", "src/./a.txt").unwrap(), "src/a.txt");
        assert_eq!(resolve("src", "../src/a.txt").unwrap(), "src/a.txt");
        assert_eq!(resolve("", "../outside.txt"), Err(76));
        assert_eq!(resolve("", "/etc/passwd"), Err(76));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.join("outside.txt"), dir.join("package/link")).unwrap();
            assert_eq!(resolve("", "link"), Err(76));
        }
        _ = std::fs::remove_dir_all(&dir);
    }

    fn files_resolve(files: &wasi::Files, rel: &str, path: &str) -> Result<String, i32> {
        files.resolve_for_test(rel, path)
    }

    /// What a module that looked at `schema.json` and `missing` and listed `src` of the package
    /// at `package` read, as the host traced it.
    fn traced_reading(package: &Path) -> Traced {
        let mut files = wasi::Files::new(Some(package));
        files.looked("schema.json", &package.join("schema.json"));
        files.looked("missing", &package.join("missing"));
        let names: Vec<OsString> = std::fs::read_dir(package.join("src"))
            .unwrap()
            .map(|it| it.unwrap().file_name())
            .collect();
        files.listed("src", names.iter().map(OsString::as_os_str));
        assert!(files.unreadable.is_none());
        files.take_traced()
    }

    fn now<'a>(root: Option<&'a Path>, room: &'a mut Room) -> Now<'a> {
        static NONE: std::sync::LazyLock<inputs::Env> = std::sync::LazyLock::new(inputs::Env::default);
        let machine = inputs::Now { files: Files::shared(), env: &NONE };
        let Room { answers, unanswered, real, all } = room;
        Now { root, machine, env: &[], answers, unanswered, real, all }
    }

    fn now_of(package: &Path, traced: &Traced) -> Traced {
        let mut room = Room::default();
        let mut now = now(Some(package), &mut room);
        index::Shape::of(traced).values(&mut now).unwrap()
    }

    #[test]
    fn the_host_traces_each_read_by_the_path_the_module_has() {
        let dir = scratch("traced");
        let package = dir.join("package");
        std::fs::create_dir_all(package.join("src")).unwrap();
        std::fs::write(package.join("schema.json"), "1").unwrap();
        std::fs::write(package.join("src/lib.rs"), "").unwrap();
        let traced = traced_reading(&package);
        let path = |rel: &str| PathBuf::from(format!("{}/{rel}", wasi::PACKAGE));
        assert_eq!(
            traced.files,
            [
                (path("missing"), None),
                (path("schema.json"), Some(blaze_ids::b3::hash(b"1"))),
            ]
        );
        let listing = blaze_usercode::trace::listing([OsStr::new("lib.rs")]);
        assert_eq!(traced.dirs, [(path("src"), listing)]);
        assert!(traced.untraceable.is_empty());
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_expansion_is_found_while_what_it_read_is_as_it_was() {
        let dir = scratch("keyed");
        let package = dir.join("package");
        std::fs::create_dir_all(package.join("src")).unwrap();
        std::fs::write(package.join("schema.json"), "1").unwrap();
        std::fs::write(package.join("src/lib.rs"), "").unwrap();
        let traced = traced_reading(&package);
        let run = RunKey { code: code(&[1; 32]), given: ContentId::of("test", b"given") };
        let key = index::result_key(&run, &traced);
        assert_eq!(index::result_key(&run, &now_of(&package, &traced)), key);
        std::fs::write(package.join("schema.json"), "22").unwrap();
        assert_ne!(index::result_key(&run, &now_of(&package, &traced)), key, "a file it read changed");
        std::fs::write(package.join("schema.json"), "1").unwrap();
        std::fs::write(package.join("missing"), "").unwrap();
        assert_ne!(index::result_key(&run, &now_of(&package, &traced)), key, "what it found missing is there");
        std::fs::remove_file(package.join("missing")).unwrap();
        std::fs::write(package.join("src/more.rs"), "").unwrap();
        assert_ne!(index::result_key(&run, &now_of(&package, &traced)), key, "a listing changed");
        std::fs::remove_file(package.join("src/more.rs")).unwrap();
        // The same package elsewhere, as on another machine, finds it.
        let moved = dir.join("moved");
        std::fs::rename(&package, &moved).unwrap();
        assert_eq!(index::result_key(&run, &now_of(&moved, &traced)), key);
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_question_of_a_span_is_an_input_with_its_answer() {
        let mut state = State::empty();
        let line = SubResponse::LineColumnResult { line: 3, column: 4 };
        state.asks.push(Value::LineColumn, [3, 9, 0, 0], Some(&line));
        let traced = what_it_read(&mut state).unwrap();
        assert!(state.asks.is_empty(), "what it asked is taken");
        assert_eq!(traced.env, [(OsString::from("\0l3,9,0,0"), Some(OsString::from("3:4")))]);
        let asked = span(3, 9);
        let shapes = Shapes::of(&[(asked.anchor, asked.ctx)], [NONE; 3]);
        let read = traced.env[0].0.to_str().unwrap().to_owned();
        let mut room = Room::default();
        // A question with no answer yet is noted, once, for the database to answer with no
        // guard held, and is no input until then.
        let mut asking = now(None, &mut room);
        assert_eq!(asking.env(&traced.env[0].0), None);
        assert_eq!(asking.env(&traced.env[0].0), None);
        let unanswered: Vec<&str> = room.unanswered.iter().map(|it| it.0).collect();
        assert_eq!(unanswered, [read.as_str()]);
        // No database answers nothing, which is not the answer it was kept with.
        room.answers.answer(&read, &shapes, None);
        assert_eq!(room.answers.get(&read), Some("-"));
        room.unanswered.clear();
        let mut answered = now(None, &mut room);
        assert_eq!(answered.env(&traced.env[0].0), Some(OsStr::new("-")));
        assert!(room.unanswered.is_empty());
        let mut state = State::empty();
        state.asked = true;
        assert!(what_it_read(&mut state).is_none(), "it learned what no question says");
    }

    #[test]
    fn a_read_of_the_clock_or_randomness_is_not_kept() {
        let traced = Traced { untraceable: vec![Untraceable::Random], ..Traced::default() };
        assert!(index::untraceable(&traced));
        let (mut one, mut two) = ([0u8; 13], [0u8; 13]);
        stdx::rand::fill(&mut one);
        stdx::rand::fill(&mut two);
        assert_ne!(one, two);
    }

    #[test]
    fn a_path_outside_the_package_is_no_input_of_a_module() {
        let mut room = Room::default();
        let mut now = now(Some(Path::new("/p")), &mut room);
        assert!(now.file(Path::new("/etc/passwd")).is_err());
    }

    /// A module whose one macro asks the host for its call site, joins it with itself and
    /// gives back its input as it was given: the host's span parts, its read and write of
    /// trees and calls into the module and out of it, with nothing of the guest's `proc_macro`.
    fn echo_module() -> Vec<u8> {
        fn name(text: &str) -> Vec<u8> {
            let mut out = vec![text.len() as u8];
            out.extend_from_slice(text.as_bytes());
            out
        }
        fn section(id: u8, body: Vec<u8>) -> Vec<u8> {
            assert!(body.len() < 128, "a length of one LEB128 byte");
            let mut out = vec![id, body.len() as u8];
            out.extend(body);
            out
        }
        let (i32, i64) = (0x7f, 0x7e);
        let types: [&[u8]; 5] = [
            // 0: `output(ptr, len)`.
            &[0x60, 2, i32, i32, 0],
            // 1: `call_site() -> span`.
            &[0x60, 0, 1, i64],
            // 2: `span_join(a, b) -> span`.
            &[0x60, 2, i64, i64, 1, i64],
            // 3: `alloc(len) -> ptr`.
            &[0x60, 1, i32, 1, i32],
            // 4: `expand(index, attr_ptr, attr_len, ptr, len)`.
            &[0x60, 5, i32, i32, i32, i32, i32, 0],
        ];
        let mut typed = vec![types.len() as u8];
        for it in types {
            typed.extend_from_slice(it);
        }
        let mut module = b"\0asm\x01\0\0\0".to_vec();
        module.extend(section(1, typed));
        let mut imports = vec![3];
        for (field, ty) in [("output", 0), ("call_site", 1), ("span_join", 2)] {
            imports.extend(name(HOST));
            imports.extend(name(field));
            imports.extend([0, ty]);
        }
        module.extend(section(2, imports));
        // Functions 3 and 4, after the three imported.
        module.extend(section(3, vec![2, 3, 4]));
        module.extend(section(5, vec![1, 0, 1]));
        let mut exported = vec![3];
        for (field, kind, index) in [("memory", 2, 0), (exports::ALLOC, 0, 3), (exports::EXPAND, 0, 4)] {
            exported.extend(name(field));
            exported.extend([kind, index]);
        }
        module.extend(section(7, exported));
        // `alloc`: 1024, where the one input goes. `expand`: `span_join(call_site(),
        // call_site())`, dropped, then `output(ptr, len)`.
        let alloc = [0, 0x41, 0x80, 0x08, 0x0b];
        let expand = [0, 0x10, 1, 0x10, 1, 0x10, 2, 0x1a, 0x20, 3, 0x20, 4, 0x10, 0, 0x0b];
        let mut code = vec![2, alloc.len() as u8];
        code.extend(alloc);
        code.push(expand.len() as u8);
        code.extend(expand);
        module.extend(section(10, code));
        module
    }

    /// The echo module ([`echo_module`]) compiled, its path in `dir`.
    fn echo(dir: &Path) -> &'static Wasm {
        let path = dir.join("echo.wasm");
        std::fs::write(&path, echo_module()).unwrap();
        Wasm::at(&path, None).unwrap()
    }

    fn echoed() -> Listed {
        Listed { kind: ProcMacroKind::Bang, name: "echo".to_owned(), helpers: Vec::new() }
    }

    /// How many allocations reading `input` back as an expansion's output takes, its parts
    /// those `call`'s sites and input give: what the output owns.
    fn owned_by_output(input: &tt::TopSubtree, call: &Call<'_>) -> u64 {
        let mut parts = Parts::default();
        for site in [call.def_site, call.call_site, call.mixed_site] {
            parts.pack(site);
        }
        let mut bytes = Vec::new();
        wire::relative(input, &mut parts, &mut bytes);
        let (read, used) = blaze_alloc_count::measure(|| wire::read(&bytes, &parts));
        assert!(read.unwrap() == *input);
        used.allocations
    }

    #[test]
    fn a_compiled_module_is_found_by_its_digest_with_no_allocation() {
        let dir = scratch("found");
        let wasm = echo(&dir);
        let path = dir.join("echo.wasm");
        let known = Known { digest: wasm.digest, identity: None };
        let again = blaze_alloc_count::assert_at_most(0, "a compiled module found by its digest", || {
            Wasm::known(&path, known, None, None, wasm.kind)
        });
        assert!(std::ptr::eq(again.unwrap(), wasm), "the module compiled once");
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_warm_expansion_in_a_kept_session_allocates_only_its_output() {
        let dir = scratch("session");
        let wasm = echo(&dir);
        let env = Env::default();
        let mut session = wasm.session(&env).unwrap();
        let listed = echoed();
        let input = tokens("a + (b, 1) \"s\"", span(4, 18)).unwrap();
        let call = Call {
            subtree: &input,
            attrs: None,
            env: &env,
            def_site: span(0, 1),
            call_site: span(1, 2),
            mixed_site: span(2, 3),
        };
        // Warms the session's span parts, this thread's buffers and the module's memory.
        for _ in 0..2 {
            let out = session.expand(0, &listed, None, &call).output.unwrap();
            assert!(out == input, "{out} came back of {input}");
        }
        let owned = owned_by_output(&input, &call);
        let ran = blaze_alloc_count::assert_at_most(owned, "a warm expansion in a kept session", || {
            session.expand(0, &listed, None, &call)
        });
        assert!(ran.output.unwrap() == input);
        assert!(!ran.found && !ran.retained);
        _ = std::fs::remove_dir_all(&dir);
    }

    /// A module whose expansions keep no state runs each in a fresh instance in a slot of the
    /// pool: it gives what a session gives, finds its calls by index, and gives its slot back.
    /// Its machine code is the session engine's, under the same key.
    #[test]
    fn a_fresh_instance_in_the_pool_expands_as_a_session_does() {
        let dir = scratch("pooled");
        let session = echo(&dir);
        let fresh = Wasm::at_kind(&dir.join("echo.wasm"), None, Kind::Fresh).unwrap();
        assert_eq!((session.kind, fresh.kind), (Kind::Session, Kind::Fresh));
        assert_eq!(
            code_key(&fresh.digest, engine_of(Kind::Fresh)),
            code_key(&session.digest, engine_of(Kind::Session)),
            "one machine code serves both engines"
        );
        let env = Env::default();
        let listed = echoed();
        let input = tokens("a + (b, 1) \"s\"", span(4, 18)).unwrap();
        let call = Call {
            subtree: &input,
            attrs: None,
            env: &env,
            def_site: span(0, 1),
            call_site: span(1, 2),
            mixed_site: span(2, 3),
        };
        let mut kept = session.session(&env).unwrap();
        assert!(kept.calls.is_some(), "a session's calls are found by index");
        let want = kept.expand(0, &listed, None, &call).output.unwrap();
        for _ in 0..3 {
            let mut one = fresh.session(&env).unwrap();
            assert!(one.calls.is_some(), "a fresh instance's calls are found by index");
            let out = one.expand(0, &listed, None, &call).output.unwrap();
            assert!(out == want, "{out} of a fresh instance, {want} of a session");
        }
        assert_eq!(*FREE.free.lock().unwrap(), SLOTS, "every slot is given back");
        _ = std::fs::remove_dir_all(&dir);
    }

    /// A store in `sim`, open and ready, and what reads it for an expansion.
    fn kept_in(sim: &mut blaze_core::sim::Sim) -> blaze_cas::Store {
        let mut effects = Vec::new();
        let config = blaze_cas::Config {
            dir: blaze_core::FileName::new("store").unwrap(),
            layers: Vec::new(),
            tags: 1 << 48,
        };
        let mut store = blaze_cas::Store::open(config, &mut effects);
        blaze_core::run_until_idle(sim, &mut effects, |completion, effects| {
            store.complete(completion, effects);
        });
        assert!(store.ready());
        store
    }

    fn keeping_in(store: &blaze_cas::Store) -> blaze_expand::Keeping {
        let expansions = blaze_expand::Expansions {
            store: store.reader(),
            toolchain: ContentId::default(),
            environment: Arc::new(proc_macro_api::warm::Environment::default()),
        };
        blaze_expand::Keeping { expansions: Box::leak(Box::new(expansions)), mode: Mode::Keep }
    }

    /// A kept expansion that read a file of its package, a variable and the line of a span is
    /// found again with nothing allocated but its output, its span parts this call's, each
    /// input looked up where it is and the span's question answered into reused room. When
    /// the store has the output's bytes off a 4-byte boundary, the bridge copies them first.
    #[cfg(unix)]
    #[test]
    fn a_kept_expansion_is_found_with_nothing_allocated_but_its_output() {
        let dir = scratch("hit");
        let wasm = echo(&dir);
        let package = dir.join("package");
        std::fs::create_dir_all(&package).unwrap();
        std::fs::write(package.join("schema.json"), "1").unwrap();
        let mut env = Env::default();
        env.set("CARGO_MANIFEST_DIR", package.to_str().unwrap());
        env.set("CARGO_PKG_NAME", "pkg");
        let listed = echoed();
        let input = tokens("a + (b, 1) \"s\"", span(4, 18)).unwrap();
        let call = Call {
            subtree: &input,
            attrs: None,
            env: &env,
            def_site: span(0, 1),
            call_site: span(1, 2),
            mixed_site: span(2, 3),
        };
        let code = code(&wasm.digest);
        let module = || Ok::<_, String>(wasm);
        let mut sim = blaze_core::sim::Sim::new(5, blaze_core::sim::Faults::default());
        let mut store = kept_in(&mut sim);
        // A miss runs the module and leaves what it made to keep.
        let ran = expand(Some(keeping_in(&store)), &code, &module, 0, &listed, None, &call);
        assert!(ran.output.unwrap() == input && !ran.found);
        let put = blaze_expand::take_puts().into_iter().find(|it| it.run.code == code).unwrap();
        // Kept as having read more than the echo does, so the hit looks each up.
        let question = read_of(Value::LineColumn, [0, 1, 0, 0]);
        let mut traced = Traced {
            files: vec![(Path::new(wasi::PACKAGE).join("schema.json"), Some(blaze_ids::b3::hash(b"1")))],
            env: vec![
                ("CARGO_PKG_NAME".into(), Some("pkg".into())),
                (question.into(), Some("-".into())),
            ],
            ..Traced::default()
        };
        blaze_usercode::trace::normalize(&mut traced);
        index::keep(&mut store, &put.run, &traced, put.value).unwrap();
        let keeping = Some(keeping_in(&store));
        let hit = || expand(keeping, &code, &module, 0, &listed, None, &call);
        // Warms this thread's scratch, buffers and file stamps.
        let first = hit();
        assert!(first.found, "the kept expansion is found");
        assert!(first.output.unwrap() == input);
        let owned = owned_by_output(&input, &call);
        // A tree off a 4-byte boundary is copied into the scratch the first hit grew.
        let ran = blaze_alloc_count::assert_at_most(owned, "a kept expansion found", hit);
        assert!(ran.found && ran.output.unwrap() == input);
        // What it read changed: then it is no hit.
        std::fs::write(package.join("schema.json"), "22").unwrap();
        let later = std::time::SystemTime::now() + Duration::from_secs(5);
        let file = std::fs::File::options().write(true).open(package.join("schema.json")).unwrap();
        file.set_modified(later).unwrap();
        let ran = expand(keeping, &code, &module, 0, &listed, None, &call);
        assert!(!ran.found && ran.output.unwrap() == input);
        _ = blaze_expand::take_puts();
        _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_seats_expansions_take_turns_in_order_and_one_within_its_own_is_refused() {
        let seat = Seat::new(());
        let turn = seat.take().unwrap();
        assert!(seat.take().is_err(), "this thread's own expansion runs in the seat");
        let order = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            let waiting = scope.spawn(|| {
                let turn = seat.take().unwrap();
                order.lock().unwrap().push("waiting");
                assert!(turn.session.is_none(), "no session was given back");
            });
            while seat.queue.lock().unwrap().next < 2 {
                std::thread::yield_now();
            }
            order.lock().unwrap().push("first");
            drop(turn);
            waiting.join().unwrap();
        });
        assert_eq!(*order.lock().unwrap(), ["first", "waiting"]);
        let queue = seat.queue.lock().unwrap();
        assert_eq!((queue.next, queue.serving, queue.running), (2, 2, None));
    }
    #[test]
    fn the_environment_is_cargos_and_the_same_on_every_machine() {
        let mut env = Env::default();
        env.set("CARGO_MANIFEST_DIR", "/home/someone/pkg");
        env.set("CARGO_PKG_NAME", "pkg");
        env.set("OUT_DIR", "/home/someone/target/out");
        env.set("AWS_SECRET_ACCESS_KEY", "no");
        env.set("PATH", "/usr/bin");
        let (vars, root) = guest_env(&env);
        let names: Vec<_> = vars.into_iter().collect();
        assert_eq!(
            names,
            [
                ("CARGO_MANIFEST_DIR", wasi::PACKAGE),
                ("CARGO_PKG_NAME", "pkg"),
                ("OUT_DIR", wasi::OUT_DIR),
            ]
        );
        assert_eq!(root, Some(Path::new("/home/someone/pkg")));
        env.set("CARGO_MANIFEST_DIR", "pkg");
        assert_eq!(guest_env(&env).1, None, "a package directory that is not absolute is none");
    }
}
