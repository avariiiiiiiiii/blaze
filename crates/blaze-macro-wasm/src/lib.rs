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
    ffi::{OsStr, OsString},
    fmt,
    panic::AssertUnwindSafe,
    path::{Path, PathBuf},
    sync::{
        Arc, Condvar, Mutex, OnceLock,
        atomic::{AtomicBool, Ordering},
    },
    thread::ThreadId,
    time::{Duration, Instant},
};

use base_db::{Db, Crate, Env, Nonce};
use blaze_core::Traced;
use blaze_expand::{
    Mode, Put,
    bridge::{
        lex, spans,
        wire::{self, NONE, Part, Parts, Tree},
    },
    tally::{Mine, Tally},
};
use blaze_ids::{ContentId, Hasher, Idx, Interner, Sharded};
use blaze_usercode::{
    Given, RunKey, RunKind,
    index::{self, FindError, Found},
    inputs::{self, Files, Unreadable},
    trace::Inputs,
};
use hir_expand::proc_macro::{
    ProcMacro, ProcMacroExpander, ProcMacroExpansionError, ProcMacroKind, ProcMacroLoadResult,
};
use intern::Symbol;
use load_cargo::{SubRequest, SubResponse};
use paths::AbsPath;
use rustc_hash::FxHashMap;
use span::{
    EditionedFileId, ErasedFileAstId, FIXUP_ERASED_FILE_AST_ID_MARKER, Span, SpanAnchor,
    SyntaxContext, TextRange, TextSize,
};
use wasmtime::{
    Caller, Config, Engine, Extern, Instance, InstancePre, Linker, Memory, Module, Store,
    StoreLimits, StoreLimitsBuilder,
};


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

fn engine() -> &'static Engine {
    static ENGINE: OnceLock<Engine> = OnceLock::new();
    ENGINE.get_or_init(|| {
        let mut config = Config::new();
        config.cranelift_nan_canonicalization(true);
        config.parallel_compilation(true);
        // A target given turns off Cranelift's reading of the host's CPU features, which
        // would put them in `code_key` and part hosts of one kind (CI's runners are AMD and
        // Intel) that should share machine code.
        config.target(&target_lexicon::HOST.to_string()).expect("the host is a Wasmtime target");
        for flag in CPU_FEATURES {
            // SAFETY: each is a feature Wasmtime checks the host has before it runs code
            // compiled with it, or (the return-address signing) what the host's ABI needs.
            unsafe { config.cranelift_flag_enable(flag) };
        }
        Engine::new(&config).expect("Wasmtime's configuration for proc macros")
    })
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
    env: Vec<(OsString, OsString)>,
    /// What a function that gives text or a stream back put there, for the module's `take`.
    buffer: Vec<u8>,
    output: Option<Vec<u8>>,
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
    asks: Vec<(String, String)>,
    db: Option<SessionDb>,
    limits: StoreLimits,
}

impl State {
    pub(crate) fn empty() -> State {
        State {
            parts: Parts::default(),
            sites: [NONE; 3],
            files: wasi::Files::new(None),
            // alloc: none, an empty list until the instance runs.
            env: Vec::new(),
            // alloc: none, an empty list until the instance runs.
            buffer: Vec::new(),
            output: None,
            panic: None,
            retained: false,
            asked: false,
            // alloc: none, an empty list until the instance runs.
            anchors: Vec::new(),
            // alloc: none, an empty list until the instance runs.
            contexts: Vec::new(),
            // alloc: none, an empty list until the instance runs.
            asks: Vec::new(),
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
                let read = value.read([start, end, anchor as u32, context as u32]);
                self.asks.push((read, value.said(answer.as_ref())));
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

    fn give(&mut self, text: Option<String>) -> i32 {
        match text {
            Some(text) => {
                self.buffer = text.into_bytes();
                self.buffer.len() as i32
            }
            None => -1,
        }
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

    /// The question of the span of `shape` (as [`Shapes`] writes one) as a read set has it:
    /// a NUL first, which no path read has.
    fn read(self, [start, end, anchor, context]: [u32; 4]) -> String {
        let letter = Value::ALL.iter().find(|it| it.0 == self).map_or('?', |it| it.1);
        format!("\0{letter}{start},{end},{anchor},{context}")
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

    /// What a read set has of `answer`: the same for the same answer, short.
    fn said(self, answer: Option<&SubResponse>) -> String {
        match answer {
            Some(SubResponse::LineColumnResult { line, column }) => format!("{line}:{column}"),
            Some(SubResponse::SourceTextResult { text: Some(text) }) => {
                keyed::digest_bytes(text.as_bytes())
            }
            Some(SubResponse::ByteRangeResult { range }) => {
                format!("{}..{}", range.start, range.end)
            }
            Some(SubResponse::FilePathResult { name }) => keyed::digest_bytes(name.as_bytes()),
            Some(SubResponse::LocalFilePathResult { name: Some(name) }) => {
                keyed::digest_bytes(name.as_bytes())
            }
            _ => "-".to_owned(),
        }
    }
}

fn memory(caller: &mut Caller<'_, State>) -> wasmtime::Result<Memory> {
    match caller.get_export("memory") {
        Some(Extern::Memory(memory)) => Ok(memory),
        _ => Err(wasmtime::Error::msg("the module exports no memory")),
    }
}

/// What a module that parses text outside an expansion (in a static's initializer or its list
/// export) is told: its tokens would have no call site to be spanned at.
const NO_CALL: &str =
    "a token stream can't be parsed outside a macro's expansion: it has no call site";

/// The module's bytes at `ptr..ptr + len`, checked against its memory before anything is
/// allocated: a length is the module's to choose.
fn read_bytes(caller: &mut Caller<'_, State>, ptr: u32, len: u32) -> wasmtime::Result<Vec<u8>> {
    let memory = memory(caller)?;
    Ok(guest_slice(memory.data(&*caller), ptr, len)?.to_vec())
}

fn guest_slice(data: &[u8], ptr: u32, len: u32) -> wasmtime::Result<&[u8]> {
    let start = ptr as usize;
    start
        .checked_add(len as usize)
        .and_then(|end| data.get(start..end))
        .ok_or_else(|| {
            wasmtime::Error::msg(format!("{len} bytes at {ptr:#x} are outside the module's memory"))
        })
}

fn read_text(caller: &mut Caller<'_, State>, ptr: u32, len: u32) -> wasmtime::Result<String> {
    let memory = memory(caller)?;
    Ok(String::from_utf8_lossy(guest_slice(memory.data(&*caller), ptr, len)?).into_owned())
}

fn range(start: u32, end: u32) -> TextRange {
    TextRange::new(TextSize::new(start), TextSize::new(end.max(start)))
}

fn spanned(file_id: u32, ast_id: u32, start: u32, end: u32, ctx: u32) -> Span {
    let anchor = SpanAnchor {
        file_id: EditionedFileId::from_raw(file_id),
        ast_id: ErasedFileAstId::from_raw(ast_id),
    };
    // SAFETY: `from_u32` asks for a value `into_u32` gave of this database's contexts: every
    // caller passes a `ctx` that `load_cargo::answer` read from the running expansion's database
    // in this process. Memory safety doesn't rest on it: a wrong value panics in `from_u32` or
    // in salsa's checked lookup, or names another context's hygiene.
    Span { range: range(start, end), anchor, ctx: unsafe { SyntaxContext::from_u32(ctx) } }
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
        // alloc: the text the macro asked for.
        let text = state.span(a).map(|it| format!("{it:?}"));
        state.give(text)
    })?;
    linker.func_wrap(HOST, "span_file", |mut caller: Caller<'_, State>, a: u64| {
        let state = caller.data_mut();
        let Some(span) = state.span(a) else { return -1 };
        let name = match state.ask_value(Value::File, span) {
            Some(SubResponse::FilePathResult { name }) => name,
            // alloc: none, an empty name.
            _ => String::new(),
        };
        state.give(Some(name))
    })?;
    linker.func_wrap(HOST, "span_local_file", |mut caller: Caller<'_, State>, a: u64| {
        let state = caller.data_mut();
        let Some(span) = state.span(a) else { return -1 };
        let name = match state.ask_value(Value::LocalFile, span) {
            Some(SubResponse::LocalFilePathResult { name }) => name,
            _ => None,
        };
        state.give(name)
    })?;
    linker.func_wrap(HOST, "span_source_text", |mut caller: Caller<'_, State>, a: u64| {
        let state = caller.data_mut();
        let Some(span) = state.span(a) else { return -1 };
        let text = match state.ask_value(Value::SourceText, span) {
            Some(SubResponse::SourceTextResult { text }) => text,
            _ => None,
        };
        state.give(text)
    })?;
    linker.func_wrap(
        HOST,
        "ts_from_str",
        |mut caller: Caller<'_, State>, ptr: u32, len: u32| -> wasmtime::Result<i32> {
            let text = read_text(&mut caller, ptr, len)?;
            let state = caller.data_mut();
            let Some(call_site) = state.span(state.sites[1]) else {
                // alloc: the message of text refused.
                return Ok(-1 - state.give(Some(NO_CALL.to_owned())));
            };
            Ok(match lex::stream(&text, call_site) {
                Ok(trees) => state.give_tree(&wire::invisible(trees, call_site)),
                Err(e) => {
                    // alloc: the message of text refused.
                    let e = format!("failed to parse str to token stream: {e}");
                    -1 - state.give(Some(e))
                }
            })
        },
    )?;
    linker.func_wrap(
        HOST,
        "literal_from_str",
        |mut caller: Caller<'_, State>, ptr: u32, len: u32| -> wasmtime::Result<i32> {
            let text = read_text(&mut caller, ptr, len)?;
            let state = caller.data_mut();
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
            let buffer = std::mem::take(&mut caller.data_mut().buffer);
            let memory = memory(&mut caller)?;
            memory.write(&mut caller, to as usize, &buffer)?;
            Ok(())
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
            let bytes = read_bytes(&mut caller, ptr, len)?;
            caller.data_mut().output = Some(bytes);
            Ok(())
        },
    )?;
    linker.func_wrap(
        HOST,
        "panicked",
        |mut caller: Caller<'_, State>, ptr: u32, len: u32| -> wasmtime::Result<()> {
            let text = read_text(&mut caller, ptr, len)?;
            caller.data_mut().panic = Some(text);
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
    pre: InstancePre<State>,
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

/// A macro of a module, as [`Wasm::macros`] lists them.
#[derive(Debug, Clone)]
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
    pub fn at(path: &Path, store: Option<&blaze_store::Store>) -> Result<Arc<Wasm>, String> {
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        let known = Known { digest: keyed::sha256(&bytes), identity: None };
        Wasm::known(path, known, Some(&bytes), store)
    }

    /// As [`Wasm::at`], of a module whose digest is known: its bytes are read only if its
    /// machine code is not in the store.
    fn known(
        path: &Path,
        known: Known,
        bytes: Option<&[u8]>,
        store: Option<&blaze_store::Store>,
    ) -> Result<Arc<Wasm>, String> {
        // A module being compiled (`prepare_soon`) is waited for, not compiled again.
        type Once = Arc<OnceLock<Result<Arc<Wasm>, String>>>;
        static COMPILED: Mutex<Option<FxHashMap<[u8; 32], Once>>> = Mutex::new(None);
        let once = {
            let mut compiled = COMPILED.lock().unwrap();
            compiled.get_or_insert_default().entry(known.digest).or_default().clone()
        };
        let compiled = once.get_or_init(|| Wasm::compile(path, known, bytes, store).map(Arc::new));
        let wasm = compiled.clone()?;
        wasm.keep_code(store);
        Ok(wasm)
    }

    fn compile(
        path: &Path,
        Known { digest, identity }: Known,
        given: Option<&[u8]>,
        store: Option<&blaze_store::Store>,
    ) -> Result<Wasm, String> {
        let start = Instant::now();
        let failed = |e: wasmtime::Error| format!("{}: {e:#}", path.display());
        let read = std::cell::OnceCell::new();
        let bytes = || module_bytes(path, &digest, given, &read);
        let key = code_key(&digest);
        let stats = std::env::var_os("BLAZE_STATS").is_some();
        // Machine code is loaded only from the store, which checks what it gives against its
        // checksum: a file anyone can write is never run as code.
        let kept = store.and_then(|it| it.get_lasting(&key)).map(|code| {
            // SAFETY: `code` is what the store keeps under `key`, checked against its checksum
            // as it was read, and only `serialize` of this module on an engine of
            // `code_key`'s compatibility hash puts under `key`; Wasmtime refuses code of
            // another version, configuration or CPU. Other bytes would run as machine code.
            unsafe { Module::deserialize(engine(), &code) }.map_err(|e| {
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
                let module = Module::new(engine(), bytes()?).map_err(failed)?;
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
        let identity = match identity {
            Some(identity) => identity,
            None => made_of(path, bytes()?).unwrap_or(digest),
        };
        let compiled_in = start.elapsed();
        let kept = was_kept.into();
        let wasm = Wasm { path: path.to_owned(), digest, identity, pre, compiled_in, kept };
        wasm.keep_code(store);
        count(Counted::Loaded, start.elapsed());
        Ok(wasm)
    }

    /// Keeps its machine code in `store`, if it has not: a module compiled before the daemon
    /// told the store the toolchain is kept once it has.
    fn keep_code(&self, store: Option<&blaze_store::Store>) {
        let Some(store) = store else { return };
        if self.kept.swap(true, Ordering::Relaxed) {
            return;
        }
        if let Ok(code) = self.pre.module().serialize() {
            store.insert_lasting(&code_key(&self.digest), &code);
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
        if let Ok(initialize) = instance.get_typed_func::<(), ()>(&mut *store, "_initialize") {
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
        let mut store = Store::new(engine(), State::empty());
        store.limiter(|state| &mut state.limits);
        let instance = self.instance(&mut store).map_err(|e| format!("{e:#}"))?;
        let list = instance
            .get_typed_func::<(), ()>(&mut store, exports::LIST)
            .map_err(|e| format!("{e:#}"))?;
        list.call(&mut store, ()).map_err(|e| format!("{e:#}"))?;
        let text = store.data_mut().output.take().unwrap_or_default();
        String::from_utf8(text).map_err(|e| e.to_string())
    }
}

/// The macros of a module's list ([`Wasm::list`]).
fn listed(text: &str) -> Vec<Listed> {
    let mut macros = Vec::new();
    for line in text.lines() {
        let mut fields = line.split('\t');
        let kind = match fields.next() {
            Some("0") => ProcMacroKind::CustomDerive,
            Some("1") => ProcMacroKind::Attr,
            _ => ProcMacroKind::Bang,
        };
        let name = fields.next().unwrap_or_default().to_owned();
        let helpers = fields.next().unwrap_or_default();
        let helpers = helpers.split(',').filter(|it| !it.is_empty()).map(str::to_owned);
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
}

impl Wasm {
    pub fn session(&self, env: &Env) -> Result<Session, ProcMacroExpansionError> {
        let root = package_dir(env);
        let mut state = State::empty();
        state.files = wasi::Files::new(root.as_deref());
        state.env = owned_env(&guest_env(env));
        self.instance_with(state, root)
    }

    /// A fresh instance whose host side starts as `state`.
    fn instance_with(
        &self,
        state: State,
        root: Option<PathBuf>,
    ) -> Result<Session, ProcMacroExpansionError> {
        let mut store = Store::new(engine(), state);
        store.limiter(|state| &mut state.limits);
        let instance = self
            .instance(&mut store)
            .map_err(|e| ProcMacroExpansionError::System(format!("{e:#}")))?;
        Ok(Session { store, instance, root, failed: false })
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
        expand(&code(&self.digest), &|| Ok(self), index, listed, db, call)
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
        state.output = None;
        state.panic = None;
        state.retained = false;
        state.files.output.clear();
        // Nothing a session makes is kept, so what it read is of no use.
        state.files.take_traced();
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
        let output = self.run(index, db, &input, attr.as_deref()).and_then(|output| {
            let tree = wire::read(&output, &self.store.data().parts)
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

    fn run(
        &mut self,
        index: u32,
        db: Option<&Db>,
        input: &[u8],
        attr: Option<&[u8]>,
    ) -> Result<Vec<u8>, ProcMacroExpansionError> {
        // `run` sets `db` to `None` before it returns or resumes an unwind, so the pointer is
        // never read after the borrow it came from ends, and the session keeps no database
        // between calls.
        self.store.data_mut().db = db.map(|db| SessionDb(std::ptr::from_ref(db)));
        let ran = std::panic::catch_unwind(AssertUnwindSafe(|| {
            let system = |e: wasmtime::Error| ProcMacroExpansionError::System(format!("{e:#}"));
            let alloc = self
                .instance
                .get_typed_func::<u32, u32>(&mut self.store, exports::ALLOC)
                .map_err(system)?;
            let expand = self
                .instance
                .get_typed_func::<(u32, u32, u32, u32, u32), ()>(&mut self.store, exports::EXPAND)
                .map_err(system)?;
            let memory = self.instance.get_memory(&mut self.store, "memory").ok_or_else(|| {
                ProcMacroExpansionError::System("the module exports no memory".to_owned())
            })?;
            let mut put = |bytes: &[u8]| -> Result<u32, ProcMacroExpansionError> {
                let at = alloc.call(&mut self.store, bytes.len() as u32).map_err(system)?;
                memory.write(&mut self.store, at as usize, bytes).map_err(|e| system(e.into()))?;
                Ok(at)
            };
            let input_at = put(input)?;
            let attr_at = attr.map(&mut put).transpose()?.unwrap_or(0);
            let attr_len = attr.map_or(0, |it| it.len() as u32);
            expand
                .call(&mut self.store, (index, attr_at, attr_len, input_at, input.len() as u32))
                .map_err(|trap| {
                    let state = self.store.data_mut();
                    match state.panic.take() {
                        Some(message) => ProcMacroExpansionError::Panic(message),
                        None => ProcMacroExpansionError::System(format!(
                            "{trap:#}\n{}",
                            String::from_utf8_lossy(&state.files.output),
                        )),
                    }
                })?;
            self.store.data_mut().output.take().ok_or_else(|| {
                ProcMacroExpansionError::System("the macro gave no output".to_owned())
            })
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
/// every host of the same kind shares it.
fn code_key(digest: &[u8; 32]) -> [u8; 32] {
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
    engine().precompile_compatibility_hash().hash(&mut Fields(&mut key));
    key.done()
}

/// Writes `bytes` to `path` whole or not at all: a process that reads it while it is written
/// sees the old file or none.
fn write_whole(path: &Path, bytes: &[u8]) {
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
        && store.is_some_and(|it| it.has_lasting(&code_key(&known.digest)))
    {
        return Ok(());
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let known = Known { digest: keyed::sha256(&bytes), identity: None };
    let wasm = Wasm::known(path, known, Some(&bytes), store)?;
    list(path, &wasm, &wasm.list()?);
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
        && store.is_some_and(|it| it.has_lasting(&code_key(&known.digest)))
    {
        return Ok(());
    }
    let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let Some(text) = carried_list(&bytes) else { return prepare(path, store) };
    let digest = keyed::sha256(&bytes);
    let identity = made_of(path, &bytes).unwrap_or(digest);
    list_known(path, &digest, &identity, &text);
    let path = path.to_owned();
    let known = Known { digest, identity: Some(identity) };
    std::thread::spawn(move || {
        let start = Instant::now();
        let done = Wasm::known(&path, known, Some(&bytes), store);
        if std::env::var_os("BLAZE_STATS").is_some() {
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
    let mut linker = Linker::new(engine());
    wasi::add(&mut linker)?;
    add_host(&mut linker)?;
    wasi::rest(&mut linker, module)?;
    linker.instantiate_pre(module)
}

/// Whether `bytes` is a module the host can run: compiled by Cranelift and linked as
/// [`Wasm::at`] links it, but neither run, kept in memory, nor its machine code written.
pub fn check_module(bytes: &[u8]) -> Result<(), String> {
    let module = Module::new(engine(), bytes).map_err(|e| format!("{e:#}"))?;
    linked(&module).map(drop).map_err(|e| format!("{e:#}"))
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
                return String::from_utf8(bytes.get(named + len..end)?.to_vec()).ok();
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
        let bytes = std::fs::read(path).map_err(|e| format!("{}: {e}", path.display()))?;
        match keyed::sha256(&bytes) == *digest {
            true => Ok(bytes),
            false => Err(format!("{} changed while it was loaded", path.display())),
        }
    });
    read.as_deref().map_err(Clone::clone)
}

/// What tells that the module at `path` and the file beside it that says what it was built of
/// are the files a list was written for: each one's length, time of writing and inode. Both
/// are written whole under a new name when they change.
fn stamp(path: &Path) -> Option<String> {
    let one = |path: &Path| -> Option<String> {
        let Ok(meta) = std::fs::metadata(path) else { return Some("-".to_owned()) };
        let written = meta.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?;
        #[cfg(unix)]
        let inode = std::os::unix::fs::MetadataExt::ino(&meta);
        #[cfg(not(unix))]
        let inode = 0;
        Some(format!("{}.{}.{inode}", meta.len(), written.as_nanos()))
    };
    let mut made_of = path.as_os_str().to_owned();
    made_of.push(".made-of");
    Some(format!("{}:{}", one(path)?, one(Path::new(&made_of))?))
}

/// What the list file beside the module at `path` knows of it, and its list of macros, when
/// it was written for the files there now ([`stamp`]): then the module need not be read.
fn listing(path: &Path) -> Option<(Known, String)> {
    let text = std::fs::read_to_string(list_file(path)).ok()?;
    let (digest, rest) = text.split_once('\n')?;
    let (head, list) = rest.split_once('\n').unwrap_or((rest, ""));
    let (identity, stamped) = head.strip_prefix("@ ")?.split_once(' ')?;
    (stamp(path)? == stamped).then_some(())?;
    let known = Known { digest: unhex(digest)?, identity: Some(unhex(identity)?) };
    Some((known, list.to_owned()))
}

/// Writes the list file of the module at `path`, compiled as `wasm`, whose macros `text` lists.
fn list(path: &Path, wasm: &Wasm, text: &str) {
    list_known(path, &wasm.digest, &wasm.identity, text);
}

fn list_known(path: &Path, digest: &[u8; 32], identity: &[u8; 32], text: &str) {
    let Some(stamp) = stamp(path) else { return };
    let (digest, identity) = (hex(digest), hex(identity));
    write_whole(&list_file(path), format!("{digest}\n@ {identity} {stamp}\n{text}").as_bytes());
}

fn hex(digest: &[u8; 32]) -> String {
    digest.iter().map(|it| format!("{it:02x}")).collect()
}

fn unhex(text: &str) -> Option<[u8; 32]> {
    let mut out = [0; 32];
    if text.len() != 64 {
        return None;
    }
    for (at, byte) in out.iter_mut().enumerate() {
        *byte = u8::from_str_radix(text.get(at * 2..at * 2 + 2)?, 16).ok()?;
    }
    Some(out)
}

/// What the module of `bytes` at `path` was built of, as the file beside it says, when that
/// file was written for these bytes.
fn made_of(path: &Path, bytes: &[u8]) -> Option<[u8; 32]> {
    let mut named = path.as_os_str().to_owned();
    named.push(".made-of");
    let text = std::fs::read_to_string(PathBuf::from(named)).ok()?;
    let (key, file) = text.trim_end().split_once(' ')?;
    let mut identity = keyed::Key::new("a proc-macro module built of");
    identity.field(key.as_bytes());
    (keyed::digest_bytes(bytes) == file).then(|| identity.done())
}

/// The package directory of the crate a macro expands in, from its `CARGO_MANIFEST_DIR`.
fn package_dir(env: &Env) -> Option<PathBuf> {
    env.get("CARGO_MANIFEST_DIR").map(PathBuf::from).filter(|it| it.is_absolute())
}

/// What a module is told of its environment: cargo's variables for a crate's compilation
/// that the crate's env has, each with what it says on every machine, sorted. No other
/// variable, of the crate's or of the process's.
fn guest_env(env: &Env) -> Vec<(&str, &str)> {
    let mut out: Vec<(&str, &str)> = env
        .iter()
        .filter_map(|(name, value)| {
            let value = match name {
                "CARGO_MANIFEST_DIR" => wasi::PACKAGE,
                "CARGO_MANIFEST_PATH" => MANIFEST_PATH,
                "OUT_DIR" => wasi::OUT_DIR,
                "CARGO_CRATE_NAME" | "CARGO_PRIMARY_PACKAGE" | "CARGO_BIN_NAME" => value,
                _ if name.starts_with("CARGO_PKG_") => value,
                _ => return None,
            };
            Some((name, value))
        })
        .collect();
    out.sort_unstable();
    out
}

/// Where a module's manifest is: [`wasi::PACKAGE`]'s `Cargo.toml`.
const MANIFEST_PATH: &str = "/blaze/package/Cargo.toml";

/// The environment a running module is given, of [`guest_env`]'s.
fn owned_env(env: &[(&str, &str)]) -> Vec<(OsString, OsString)> {
    env.iter().map(|&(name, value)| (name.into(), value.into())).collect()
}

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

fn position<T: PartialEq>(all: &mut Vec<T>, it: T) -> u32 {
    let at = all.iter().position(|known| *known == it).unwrap_or_else(|| {
        all.push(it);
        all.len() - 1
    });
    at as u32
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
            let anchor = position(&mut self.anchors, anchor);
            let context = position(&mut self.contexts, ctx);
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
    fn read(&self, value: &[u8], parts: &mut Parts) -> Result<tt::TopSubtree, String> {
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
        wire::read(&value[4 + table.len()..], parts)
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
    for (read, said) in std::mem::take(&mut state.asks) {
        traced.env.push((read.into(), Some(said.into())));
    }
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
    /// The same, owned, made only for a shape that read all of it ([`Inputs::env_all`]).
    all: Option<Vec<(OsString, OsString)>>,
    shapes: &'a Shapes,
    /// The answers so far, by question as a read set has it ([`Value::read`]).
    answers: &'a FxHashMap<String, String>,
    unanswered: Vec<String>,
}

/// What a read set has for the question `read` ([`Value::read`]) of the input of `shapes`, as
/// `db` answers it: no database answers nothing ([`Value::said`]).
fn answer(read: &str, shapes: &Shapes, db: Option<&Db>) -> String {
    match Value::parse(read, shapes) {
        Some((value, span)) => {
            let answer = db.map(|db| load_cargo::answer(db, value.request(span)));
            value.said(answer.as_ref())
        }
        None => "?".to_owned(),
    }
}

impl Now<'_> {
    /// Where the package's `path` is on this machine: `None` for a module given no package,
    /// which found nothing there.
    fn real(&self, path: &Path) -> Result<Option<PathBuf>, Unreadable> {
        let rel = wasi::rel_of(path).ok_or_else(|| Unreadable {
            path: path.to_owned(),
            error: format!("a module reads only its package, at {}", wasi::PACKAGE),
        })?;
        Ok(self.root.map(|root| root.join(rel)))
    }
}

impl Inputs for Now<'_> {
    type Error = Unreadable;

    fn file(&mut self, path: &Path) -> Result<Option<[u8; 32]>, Unreadable> {
        match self.real(path)? {
            Some(real) => self.machine.file(&real),
            None => Ok(None),
        }
    }

    fn dir(&mut self, path: &Path) -> Result<Option<[u8; 32]>, Unreadable> {
        match self.real(path)? {
            Some(real) => self.machine.dir(&real),
            None => Ok(None),
        }
    }

    fn env(&mut self, name: &OsStr) -> Option<OsString> {
        if name.as_encoded_bytes().first() == Some(&0) {
            let name = name.to_str()?;
            if let Some(said) = self.answers.get(name) {
                return Some(said.into());
            }
            if !self.unanswered.iter().any(|it| it == name) {
                self.unanswered.push(name.to_owned());
            }
            return None;
        }
        let name = name.to_str()?;
        let at = self.env.binary_search_by(|it| it.0.cmp(name)).ok()?;
        Some(self.env[at].1.into())
    }

    fn env_all(&mut self) -> &[(OsString, OsString)] {
        let env = self.env;
        self.all.get_or_insert_with(|| owned_env(env))
    }

    fn names(&mut self, path: &Path) -> Result<Option<Vec<(OsString, bool)>>, Unreadable> {
        match self.real(path)? {
            Some(real) => self.machine.names(&real),
            None => Ok(None),
        }
    }
}

/// What the store has of `run` for its inputs as they are now, by `look` of what
/// [`index::find`] found, which runs with the store's guard held. A pass that met a question
/// of a span with no answer yet is thrown away: the questions are asked of `db` with no guard
/// alive, since answering one can expand other macros on this thread, and it looks again.
/// Each pass that does not end answers at least one more of the finitely many questions the
/// store's shapes of `run` ask.
fn find_answered<T>(
    expansions: &blaze_expand::Expansions,
    run: &RunKey,
    root: Option<&Path>,
    env: &[(&str, &str)],
    shapes: &Shapes,
    db: Option<&Db>,
    look: impl Fn(Found<'_>) -> T,
) -> Result<T, FindError<Unreadable>> {
    let none = inputs::Env::default();
    let machine = inputs::Now { files: Files::shared(), env: &none };
    let mut answers = FxHashMap::default();
    loop {
        let mut now =
            Now { root, machine, env, all: None, shapes, answers: &answers, unanswered: Vec::new() };
        let looked = {
            let guard = blaze_cas::enter();
            index::find(&expansions.store, &guard, run, &mut now).map(&look)
        };
        if now.unanswered.is_empty() {
            return looked;
        }
        let unanswered = std::mem::take(&mut now.unanswered);
        for read in unanswered {
            let said = answer(&read, shapes, db);
            answers.insert(read, said);
        }
    }
}

/// A buffer of this thread's, cleared, given back when dropped: a call's tokens are encoded
/// without allocating once the thread has encoded one as large.
struct Reused(Vec<u8>);

thread_local! {
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

/// An expansion's span parts and shapes and a kept output's parts, this thread's, cleared
/// where used, given back when dropped: a hit allocates none of them once the thread has
/// expanded one as large. An expansion while another runs on the thread makes its own.
#[derive(Default)]
struct Scratch {
    parts: Parts,
    shapes: Shapes,
    kept: Parts,
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
/// ([`what_it_read`]) unless it read what can't be traced or left state behind. Nothing is
/// kept where blaze-expand was not installed (a bench).
#[allow(clippy::too_many_arguments)]
fn expand<'a>(
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
    let root = package_dir(call.env);
    let env = guest_env(call.env);
    let mut scratch = Scratched::take();
    let Scratch { parts, shapes, kept } = &mut scratch.0;
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
    let restore = |tree: tt::TopSubtree| match &root {
        Some(root) => placed(tree, root),
        None => tree,
    };
    let keeping = blaze_expand::keeping();
    let kept = std::cell::Cell::from_mut(kept);
    let found = match keeping {
        Some(keeping) => {
            let found = find_answered(
                keeping.expansions,
                &run,
                root.as_deref(),
                &env,
                shapes,
                db,
                |found| match found {
                    // Read in place, under the guard: a hit copies nothing out of the store.
                    Found::Hit { value, .. } if keeping.mode == Mode::Keep => {
                        let mut parts = kept.take();
                        let read = shapes.read(value, &mut parts);
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
        state.files = wasi::Files::new(root.as_deref());
        state.env = owned_env(&env);
        // clone: the instance's own path, made only when the macro runs.
        let mut instance = wasm.instance_with(state, root.clone())?;
        let output = instance.run(index, db, &input, attr.as_deref())?;
        Ok((instance.store.into_data(), output))
    });
    let mut retained = false;
    let output = ran.and_then(|(mut state, output)| {
        let tree = wire::read(&output, &state.parts)
            // alloc: the message of an output refused.
            .map_err(|e| ProcMacroExpansionError::System(format!("`{}`: {e}", listed.name)))?;
        retained = state.retained;
        let value = shapes.written(&state.parts, &output);
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

/// What blaze has seen of a module's expansions, kept in the store under the module's digest
/// ([`verdict_key`]) for every process and machine that loads the same module.
#[derive(Clone, Copy, PartialEq, Debug)]
enum Verdict {
    /// None has run in any process that told the store.
    Unseen,
    /// Some ran, in a build, and none left what it allocated live.
    Stateless,
    /// One left what it allocated live, for a later one to find.
    Stateful,
}

const STATELESS: &[u8] = b"stateless";
const STATEFUL: &[u8] = b"stateful";

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
    compiled: OnceLock<Result<Arc<Wasm>, String>>,
    /// What the store said of it when it was loaded, which decides for as long as it is:
    /// whether a macro needs a session is asked of the database's queries, which must not
    /// see it change.
    verdict: Verdict,
    /// Whether one of its expansions ran in this process, and whether one left state.
    ran: AtomicBool,
    retained: AtomicBool,
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
            path: path.to_owned(),
            known,
            compiled: OnceLock::new(),
            verdict: verdict(&known.digest),
            ran: AtomicBool::new(false),
            retained: AtomicBool::new(false),
            seats: Sharded::new(),
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
        let made: Vec<Idx<Seat>> = self.made.lock().unwrap_or_else(|it| it.into_inner()).clone();
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

    fn get(&self) -> Result<&Arc<Wasm>, String> {
        let compiled =
            self.compiled.get_or_init(|| Wasm::known(&self.path, self.known, None, store()));
        let wasm = compiled.as_ref().map_err(Clone::clone)?;
        wasm.keep_code(store());
        Ok(wasm)
    }

    /// Whether its macros expand in a session: unless the store says none of its expansions
    /// left state.
    fn stateful(&self) -> bool {
        self.verdict != Verdict::Stateless
    }

    /// Notes what an expansion that ran did. One that left state is told the store at once;
    /// that none did, by [`flush`], once a build has run them.
    fn saw(&self, ran: &Ran) {
        if ran.found || ran.output.is_err() {
            return;
        }
        if !ran.retained {
            self.ran.store(true, Ordering::Relaxed);
            return;
        }
        if self.retained.swap(true, Ordering::Relaxed) || self.verdict == Verdict::Stateful {
            return;
        }
        if let Some(store) = store() {
            store.insert_lasting(&verdict_key(&self.known.digest), STATEFUL);
            store.flush();
        }
        if !self.stateful() {
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
            said += &format!("; {calls} {what}");
        }
    }
    let modules = MODULES.lock().unwrap_or_else(|e| e.into_inner());
    let sessions = modules.iter().filter_map(std::sync::Weak::upgrade).filter(|it| it.stateful());
    let sessions = sessions.count();
    if sessions != 0 {
        said += &format!("; {sessions} modules expand in sessions (keep state or not seen yet)");
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
        if lazy.verdict == Verdict::Unseen
            && lazy.ran.swap(false, Ordering::Relaxed)
            && !lazy.retained.load(Ordering::Relaxed)
            && verdict(&lazy.known.digest) == Verdict::Unseen
        {
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
        subtree: &tt::TopSubtree,
        attrs: Option<&tt::TopSubtree>,
        env: &Env,
        def_site: Span,
        call_site: Span,
        mixed_site: Span,
        _current_dir: String,
    ) -> Result<tt::TopSubtree, ProcMacroExpansionError> {
        if self.wasm.stateful() {
            return Err(ProcMacroExpansionError::System(
                "a proc macro that keeps state expands only for a calling crate".to_owned(),
            ));
        }
        let call = Call { subtree, attrs, env, def_site, call_site, mixed_site };
        // What its expansions are kept under is known without the module: then one the store
        // has neither loads nor compiles it.
        let wasm = || self.wasm.get().map(|it| &**it);
        let code = code(&self.wasm.known.digest);
        let ran = expand(&code, &wasm, self.index, &self.listed, Some(db), &call);
        self.wasm.saw(&ran);
        ran.output
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
        if !self.wasm.stateful() {
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
        // A new revision must rerun unchanged registrations before changed uses.
        db.report_untracked_read();
        let (nonce, revision) = db.nonce_and_revision();
        let wasm = self.wasm.get().map_err(ProcMacroExpansionError::System)?;
        let seat = self.wasm.seat((nonce, calling_crate));
        let mut turn = seat.take().map_err(|()| {
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
        let session = session.as_mut().map_err(|e| ProcMacroExpansionError::System(e.clone()))?;
        let call = Call { subtree, attrs, env, def_site, call_site, mixed_site };
        let ran = session.expand(self.index, &self.listed, Some(db), &call);
        self.wasm.saw(&ran);
        ran.output
    }

    fn eq_dyn(&self, other: &dyn ProcMacroExpander) -> bool {
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
            format!("proc macros compiled to WebAssembly: {e}").into(),
        )
    };
    let path: &Path = path.as_ref();
    let (lazy, text) = match listing(path) {
        Some((known, text)) => (Lazy::new(path, known), text),
        None => {
            let wasm = Wasm::at(path, store()).map_err(failed)?;
            let text = wasm.list().map_err(failed)?;
            list(path, &wasm, &text);
            let known = Known { digest: wasm.digest, identity: Some(wasm.identity) };
            let lazy = Lazy::new(path, known);
            _ = lazy.compiled.set(Ok(wasm));
            (lazy, text)
        }
    };
    let lazy = Arc::new(lazy);
    MODULES.lock().unwrap_or_else(|e| e.into_inner()).push(Arc::downgrade(&lazy));
    Ok(listed(&text)
        .into_iter()
        .enumerate()
        .map(|(index, listed)| ProcMacro {
            name: Symbol::intern(&listed.name),
            kind: listed.kind,
            expander: Arc::new(Expander {
                wasm: AssertUnwindSafe(lazy.clone()),
                index: index as u32,
                listed,
            }),
            disabled: false,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use blaze_core::Untraceable;

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
        Wasm::known(&path, known, None, Some(store)).unwrap();
        let key = code_key(&known.digest);
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
        let read = Value::LineColumn.read([3, 9, 0, 0]);
        let (value, again) = Value::parse(&read, &of(asked)).unwrap();
        assert!(value == Value::LineColumn);
        assert_eq!(again, asked);
        // The next call's span in the same place, of another anchor.
        let mut moved = asked;
        moved.anchor.ast_id = ErasedFileAstId::from_raw(5);
        assert_eq!(Value::parse(&read, &of(moved)).unwrap().1, moved);
        assert!(Value::parse(&Value::File.read([0, 0, 1, 0]), &of(asked)).is_none());
        assert!(Value::parse("src/lib.rs", &of(asked)).is_none());
        let answer = SubResponse::LineColumnResult { line: 3, column: 4 };
        assert_eq!(Value::LineColumn.said(Some(&answer)), "3:4");
        assert_eq!(Value::SourceText.said(None), "-");
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
        files.listed("src", &names);
        assert!(files.unreadable.is_none());
        files.take_traced()
    }

    fn now<'a>(
        root: Option<&'a Path>,
        shapes: &'a Shapes,
        answers: &'a FxHashMap<String, String>,
    ) -> Now<'a> {
        static NONE: std::sync::LazyLock<inputs::Env> = std::sync::LazyLock::new(inputs::Env::default);
        let machine = inputs::Now { files: Files::shared(), env: &NONE };
        Now { root, machine, env: &[], all: None, shapes, answers, unanswered: Vec::new() }
    }

    fn now_of(package: &Path, traced: &Traced) -> Traced {
        let shapes = Shapes::of(&[], [NONE; 3]);
        let answers = FxHashMap::default();
        let mut now = now(Some(package), &shapes, &answers);
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
                (path("schema.json"), Some(*blake3::hash(b"1").as_bytes())),
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
        state.asks.push((Value::LineColumn.read([3, 9, 0, 0]), "3:4".to_owned()));
        let traced = what_it_read(&mut state).unwrap();
        assert_eq!(traced.env.len(), 1);
        let asked = span(3, 9);
        let shapes = Shapes::of(&[(asked.anchor, asked.ctx)], [NONE; 3]);
        let read = traced.env[0].0.to_str().unwrap().to_owned();
        let mut answers = FxHashMap::default();
        // A question with no answer yet is noted, once, for the database to answer with no
        // guard held, and is no input until then.
        let mut asking = now(None, &shapes, &answers);
        assert_eq!(asking.env(&traced.env[0].0), None);
        assert_eq!(asking.env(&traced.env[0].0), None);
        assert_eq!(asking.unanswered, [read.clone()]);
        // No database answers nothing, which is not the answer it was kept with.
        let said = answer(&read, &shapes, None);
        assert_eq!(said, "-");
        answers.insert(read, said);
        let mut answered = now(None, &shapes, &answers);
        assert_eq!(answered.env(&traced.env[0].0), Some("-".into()));
        assert!(answered.unanswered.is_empty());
        let mut state = State::empty();
        state.asked = true;
        assert!(what_it_read(&mut state).is_none(), "it learned what no question says");
    }

    #[test]
    fn a_read_of_the_clock_or_randomness_is_not_kept() {
        let traced = Traced { untraceable: vec![Untraceable::Random], ..Traced::default() };
        assert!(index::untraceable(&traced));
        let (mut one, mut two) = ([0u8; 13], [0u8; 13]);
        wasi::fill_random(&mut one);
        wasi::fill_random(&mut two);
        assert_ne!(one, two);
    }

    #[test]
    fn a_path_outside_the_package_is_no_input_of_a_module() {
        let shapes = Shapes::of(&[], [NONE; 3]);
        let answers = FxHashMap::default();
        let mut now = now(Some(Path::new("/p")), &shapes, &answers);
        assert!(now.file(Path::new("/etc/passwd")).is_err());
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
        let names: Vec<_> = guest_env(&env).into_iter().collect();
        assert_eq!(
            names,
            [
                ("CARGO_MANIFEST_DIR", wasi::PACKAGE),
                ("CARGO_PKG_NAME", "pkg"),
                ("OUT_DIR", wasi::OUT_DIR),
            ]
        );
    }
}
