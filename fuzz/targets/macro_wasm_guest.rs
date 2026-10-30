//! Not a fuzz target: what the `macro_wasm_` targets share, each including it as
//! `#[path = "macro_wasm_guest.rs"] mod guest;`. The WebAssembly modules they run, written out
//! byte by byte (the package has no wasm encoder), and the host each runs one in: a module of
//! proc macros loaded as blaze loads one ([`Wasm::at`]), whose package is a temporary
//! directory with one file, `input`, the fuzzer's input, which the module reads through the
//! host's WASI at every instance.
//!
//! [`Kind::Interpreter`] runs the input as a program of host calls: a phase byte
//! ([`IN_INITIALIZE`], [`FRESH`], [`ATTR`]), a count of records, then the records, each an
//! operation byte and [`ARGS`] little-endian `i32`s, then data the records may point at
//! ([`data_at`]). An operation is [`OPS`]-modular: a store, a fill or a copy of the module's
//! memory, or a call of one of [`IMPORTS`] with the record's arguments (an `i64` one
//! sign-extended), its results dropped. [`Kind::Echo`] gives the input back as its output.

use std::{
    path::PathBuf,
    sync::Mutex,
};

use base_db::Env;
use blaze_macro_wasm::{Call, Listed, Wasm, exports};
use hir_expand::proc_macro::{ProcMacroExpansionError, ProcMacroKind};
use span::{
    Edition, EditionedFileId, FileId, ROOT_ERASED_FILE_AST_ID, Span, SpanAnchor, SyntaxContext,
    TextRange, TextSize,
};

/// The module's memory, in 64 KiB pages: it neither grows nor shrinks.
pub(crate) const PAGES: u32 = 4;
pub(crate) const MEMORY: usize = PAGES as usize * 65536;

const FD_AT: i32 = 0;
const NREAD_AT: i32 = 4;
/// An iovec of the input buffer: `BUF`, `CAP`.
pub(crate) const IOV_AT: i32 = 8;
/// `input`, the package's file the module reads.
pub(crate) const PATH_AT: i32 = 16;
pub(crate) const PATH: &[u8] = b"input";
const LIST_AT: i32 = 32;
const LIST: &[u8] = b"2\tfuzz\n";
/// Where `blaze_proc_macro_alloc` puts what the host gives the module.
pub(crate) const ALLOC_AT: i32 = 0x100;
/// Where the module reads its input, and how much of it.
pub(crate) const BUF: i32 = 0x8000;
pub(crate) const CAP: i32 = MEMORY as i32 - BUF;

/// The arguments of a record, and its length.
pub(crate) const ARGS: usize = 9;
pub(crate) const RECORD: usize = 1 + 4 * ARGS;

/// Phase bits: the program runs in `_initialize` rather than in the expansion.
pub(crate) const IN_INITIALIZE: u8 = 1;
/// The expansion runs in a fresh instance ([`Wasm::expand`]) rather than a [`Session`].
///
/// [`Session`]: blaze_macro_wasm::Session
pub(crate) const FRESH: u8 = 2;
/// The macro is an attribute, given an empty attribute stream.
pub(crate) const ATTR: u8 = 4;

const PROC_MACRO: &str = "blaze_proc_macro";
const WASI: &str = "wasi_snapshot_preview1";

#[derive(Clone, Copy, PartialEq)]
pub(crate) enum Ty {
    I32,
    I64,
}

use Ty::{I32, I64};

/// What the module imports: every function of the host's (`add_host`, `wasi::add`), with its
/// type as the host defines it, and two of WASI's the host answers `ENOSYS` to (`wasi::rest`).
pub(crate) const IMPORTS: &[(&str, &str, &[Ty], &[Ty])] = &[
    (PROC_MACRO, "def_site", &[], &[I32]),
    (PROC_MACRO, "call_site", &[], &[I32]),
    (PROC_MACRO, "mixed_site", &[], &[I32]),
    (PROC_MACRO, "span_join", &[I32, I32], &[I32]),
    (PROC_MACRO, "span_resolved_at", &[I32, I32], &[I32]),
    (PROC_MACRO, "span_located_at", &[I32, I32], &[I32]),
    (PROC_MACRO, "span_parent", &[I32], &[I32]),
    (PROC_MACRO, "span_source", &[I32], &[I32]),
    (PROC_MACRO, "span_start", &[I32], &[I32]),
    (PROC_MACRO, "span_end", &[I32], &[I32]),
    (PROC_MACRO, "span_subspan", &[I32, I64, I64], &[I32]),
    (PROC_MACRO, "span_byte_range", &[I32, I32], &[]),
    (PROC_MACRO, "span_line", &[I32], &[I32]),
    (PROC_MACRO, "span_column", &[I32], &[I32]),
    (PROC_MACRO, "span_debug", &[I32], &[I32]),
    (PROC_MACRO, "span_file", &[I32], &[I32]),
    (PROC_MACRO, "span_local_file", &[I32], &[I32]),
    (PROC_MACRO, "span_source_text", &[I32], &[I32]),
    (PROC_MACRO, "ts_from_str", &[I32, I32], &[I32]),
    (PROC_MACRO, "literal_from_str", &[I32, I32], &[I32]),
    (PROC_MACRO, "take", &[I32], &[]),
    (PROC_MACRO, "track_path", &[I32, I32], &[]),
    (PROC_MACRO, "track_env_var", &[I32, I32], &[]),
    (PROC_MACRO, "diagnostic", &[I32, I32, I32, I32], &[]),
    (PROC_MACRO, "output", &[I32, I32], &[]),
    (PROC_MACRO, "panicked", &[I32, I32], &[]),
    (PROC_MACRO, "retained", &[I32], &[]),
    (WASI, "args_sizes_get", &[I32, I32], &[I32]),
    (WASI, "args_get", &[I32, I32], &[I32]),
    (WASI, "environ_sizes_get", &[I32, I32], &[I32]),
    (WASI, "environ_get", &[I32, I32], &[I32]),
    (WASI, "clock_res_get", &[I32, I32], &[I32]),
    (WASI, "clock_time_get", &[I32, I64, I32], &[I32]),
    (WASI, "random_get", &[I32, I32], &[I32]),
    (WASI, "sched_yield", &[], &[I32]),
    (WASI, "proc_exit", &[I32], &[]),
    (WASI, "fd_write", &[I32, I32, I32, I32], &[I32]),
    (WASI, "fd_read", &[I32, I32, I32, I32], &[I32]),
    (WASI, "fd_pread", &[I32, I32, I32, I64, I32], &[I32]),
    (WASI, "fd_close", &[I32], &[I32]),
    (WASI, "fd_seek", &[I32, I64, I32, I32], &[I32]),
    (WASI, "fd_tell", &[I32, I32], &[I32]),
    (WASI, "fd_fdstat_get", &[I32, I32], &[I32]),
    (WASI, "fd_fdstat_set_flags", &[I32, I32], &[I32]),
    (WASI, "fd_filestat_get", &[I32, I32], &[I32]),
    (WASI, "fd_prestat_get", &[I32, I32], &[I32]),
    (WASI, "fd_prestat_dir_name", &[I32, I32, I32], &[I32]),
    (WASI, "path_open", &[I32, I32, I32, I32, I32, I64, I64, I32, I32], &[I32]),
    (WASI, "path_filestat_get", &[I32, I32, I32, I32, I32], &[I32]),
    (WASI, "path_readlink", &[I32, I32, I32, I32, I32, I32], &[I32]),
    (WASI, "fd_readdir", &[I32, I32, I32, I64, I32], &[I32]),
    (WASI, "fd_sync", &[I32], &[I32]),
    (WASI, "poll_oneoff", &[I32, I32, I32, I32], &[I32]),
];

/// The operations of a record: a store, a fill and a copy of memory, then each import.
pub(crate) const OPS: u32 = 3 + IMPORTS.len() as u32;

/// The operation byte of `name`: `store32`, `fill`, `copy` or an import's name.
pub(crate) fn op(name: &str) -> u8 {
    match name {
        "store32" => 0,
        "fill" => 1,
        "copy" => 2,
        _ => {
            let at = IMPORTS.iter().position(|it| it.1 == name);
            3 + at.unwrap_or_else(|| panic!("`{name}` is no import of the module")) as u8
        }
    }
}

/// Where the data after `records` records is in the module's memory.
pub(crate) fn data_at(records: usize) -> i32 {
    BUF + 2 + (RECORD * records) as i32
}

/// A program of `records`, each an operation's name and its arguments, then `data`.
pub(crate) fn program(phase: u8, records: &[(&str, [i32; ARGS])], data: &[u8]) -> Vec<u8> {
    let count = u8::try_from(records.len()).expect("a program has at most 255 records");
    let mut out = vec![phase, count];
    for (name, args) in records {
        out.push(op(name));
        for arg in args {
            out.extend_from_slice(&arg.to_le_bytes());
        }
    }
    out.extend_from_slice(data);
    out
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) enum Kind {
    Interpreter,
    Echo,
}

fn uleb(out: &mut Vec<u8>, mut value: u64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if value == 0 {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn sleb(out: &mut Vec<u8>, mut value: i64) {
    loop {
        let byte = (value & 0x7f) as u8;
        value >>= 7;
        if (value == 0 && byte & 0x40 == 0) || (value == -1 && byte & 0x40 != 0) {
            out.push(byte);
            return;
        }
        out.push(byte | 0x80);
    }
}

fn name(out: &mut Vec<u8>, text: &[u8]) {
    uleb(out, text.len() as u64);
    out.extend_from_slice(text);
}

fn section(module: &mut Vec<u8>, id: u8, body: &[u8]) {
    module.push(id);
    uleb(module, body.len() as u64);
    module.extend_from_slice(body);
}

fn valtypes(out: &mut Vec<u8>, types: &[Ty]) {
    uleb(out, types.len() as u64);
    for ty in types {
        out.push(match ty {
            I32 => 0x7f,
            I64 => 0x7e,
        });
    }
}

const BLOCK: u8 = 0x02;
const LOOP: u8 = 0x03;
const IF: u8 = 0x04;
const END: u8 = 0x0b;
const BR: u8 = 0x0c;
const BR_IF: u8 = 0x0d;
const BR_TABLE: u8 = 0x0e;
const RETURN: u8 = 0x0f;
const DROP: u8 = 0x1a;
const SELECT: u8 = 0x1b;
const I32_LOAD: u8 = 0x28;
const I32_LOAD8_U: u8 = 0x2d;
const I32_STORE: u8 = 0x36;
const I32_EQZ: u8 = 0x45;
const I32_LT_U: u8 = 0x49;
const I32_GT_U: u8 = 0x4b;
const I32_GE_S: u8 = 0x4e;
const I32_ADD: u8 = 0x6a;
const I32_MUL: u8 = 0x6c;
const I32_REM_U: u8 = 0x70;
const I32_AND: u8 = 0x71;
const I64_EXTEND_I32_S: u8 = 0xac;
/// The empty block type.
const EMPTY: u8 = 0x40;
/// The prefix of `memory.copy` (10) and `memory.fill` (11).
const MISC: u8 = 0xfc;

/// A function's instructions.
#[derive(Default)]
struct Code(Vec<u8>);

impl Code {
    fn op(&mut self, bytes: &[u8]) -> &mut Code {
        self.0.extend_from_slice(bytes);
        self
    }

    fn index(&mut self, it: u32) -> &mut Code {
        uleb(&mut self.0, it.into());
        self
    }

    fn i32(&mut self, value: i32) -> &mut Code {
        self.0.push(0x41);
        sleb(&mut self.0, value.into());
        self
    }

    fn i64(&mut self, value: i64) -> &mut Code {
        self.0.push(0x42);
        sleb(&mut self.0, value);
        self
    }

    fn get(&mut self, local: u32) -> &mut Code {
        self.op(&[0x20]).index(local)
    }

    fn set(&mut self, local: u32) -> &mut Code {
        self.op(&[0x21]).index(local)
    }

    fn tee(&mut self, local: u32) -> &mut Code {
        self.op(&[0x22]).index(local)
    }

    fn call(&mut self, func: u32) -> &mut Code {
        self.op(&[0x10]).index(func)
    }

    /// A load or store at the address on the stack plus `offset`, of alignment 1.
    fn mem(&mut self, opcode: u8, offset: u32) -> &mut Code {
        self.op(&[opcode, 0]).index(offset)
    }

    /// The function of these instructions with `locals` locals of `i32` after its parameters.
    fn body(&self, locals: u32) -> Vec<u8> {
        let mut out = Vec::new();
        match locals {
            0 => out.push(0),
            _ => {
                out.push(1);
                uleb(&mut out, locals.into());
                out.push(0x7f);
            }
        }
        out.extend_from_slice(&self.0);
        out.push(END);
        out
    }
}

fn import(name: &str) -> u32 {
    let at = IMPORTS.iter().position(|it| it.1 == name);
    at.unwrap_or_else(|| panic!("`{name}` is no import of the module")) as u32
}

/// The module of `kind`, which links against the host's imports.
pub(crate) fn module(kind: Kind) -> Vec<u8> {
    let imports = IMPORTS.len() as u32;
    let [load, run, initialize, alloc, list, expand] = [0, 1, 2, 3, 4, 5].map(|it| imports + it);
    let own: [(&[Ty], &[Ty]); 6] = [
        (&[], &[I32]),
        (&[I32], &[]),
        (&[], &[]),
        (&[I32], &[I32]),
        (&[], &[]),
        (&[I32, I32, I32, I32, I32], &[]),
    ];
    let mut out = b"\0asm\x01\0\0\0".to_vec();

    // A type per import, then a type per function of the module's, in order.
    let mut types = Vec::new();
    uleb(&mut types, (IMPORTS.len() + own.len()) as u64);
    for (params, results) in IMPORTS.iter().map(|it| (it.2, it.3)).chain(own) {
        types.push(0x60);
        valtypes(&mut types, params);
        valtypes(&mut types, results);
    }
    section(&mut out, 1, &types);

    let mut imported = Vec::new();
    uleb(&mut imported, IMPORTS.len() as u64);
    for (at, (module, field, ..)) in IMPORTS.iter().enumerate() {
        name(&mut imported, module.as_bytes());
        name(&mut imported, field.as_bytes());
        imported.push(0x00);
        uleb(&mut imported, at as u64);
    }
    section(&mut out, 2, &imported);

    let mut functions = Vec::new();
    uleb(&mut functions, own.len() as u64);
    for at in 0..own.len() as u32 {
        uleb(&mut functions, (imports + at).into());
    }
    section(&mut out, 3, &functions);

    let mut memory = vec![1, 1];
    uleb(&mut memory, PAGES.into());
    uleb(&mut memory, PAGES.into());
    section(&mut out, 5, &memory);

    let exported: [(&str, u8, u32); 5] = [
        ("memory", 0x02, 0),
        ("_initialize", 0x00, initialize),
        (exports::ALLOC, 0x00, alloc),
        (exports::LIST, 0x00, list),
        (exports::EXPAND, 0x00, expand),
    ];
    let mut exporting = Vec::new();
    uleb(&mut exporting, exported.len() as u64);
    for (field, what, index) in exported {
        name(&mut exporting, field.as_bytes());
        exporting.push(what);
        uleb(&mut exporting, index.into());
    }
    section(&mut out, 7, &exporting);

    // `load`: the package's `input` read into `BUF`, its length, or -1.
    let mut loading = Code::default();
    loading.i32(3).i32(0).i32(PATH_AT).i32(PATH.len() as i32).i32(0).i64(0).i64(0).i32(0);
    loading.i32(FD_AT).call(import("path_open"));
    loading.op(&[IF, EMPTY]).i32(-1).op(&[RETURN, END]);
    loading.i32(FD_AT).mem(I32_LOAD, 0).i32(IOV_AT).i32(1).i32(NREAD_AT).call(import("fd_read"));
    loading.op(&[IF, EMPTY]).i32(-1).op(&[RETURN, END]);
    loading.i32(NREAD_AT).mem(I32_LOAD, 0);

    // `run(n)`, of locals `pc` 1, `end` 2, `op` 3 and the arguments 4 on: each record of the
    // first `n` bytes at `BUF`, up to the count.
    let mut running = Code::default();
    running.i32(BUF + 2).set(1);
    running.i32(BUF + 1).mem(I32_LOAD8_U, 0).i32(RECORD as i32).op(&[I32_MUL]);
    running.i32(BUF + 2).op(&[I32_ADD]).set(2);
    running.get(2).i32(BUF).get(0).op(&[I32_ADD]);
    running.get(2).i32(BUF).get(0).op(&[I32_ADD, I32_LT_U, SELECT]).set(2);
    running.op(&[BLOCK, EMPTY, LOOP, EMPTY]);
    running.get(1).i32(RECORD as i32).op(&[I32_ADD]).get(2).op(&[I32_GT_U, BR_IF]).index(1);
    running.get(1).mem(I32_LOAD8_U, 0).set(3);
    for arg in 0..ARGS as u32 {
        running.get(1).mem(I32_LOAD, 1 + 4 * arg).set(4 + arg);
    }
    running.get(1).i32(RECORD as i32).op(&[I32_ADD]).set(1);
    // A block per operation inside one for none: `br_table` to the end of the block of the
    // operation, after which is its body, then a branch back to the loop.
    running.op(&[BLOCK, EMPTY]);
    for _ in 0..OPS {
        running.op(&[BLOCK, EMPTY]);
    }
    running.get(3).i32(OPS as i32).op(&[I32_REM_U, BR_TABLE]).index(OPS);
    for label in 0..=OPS {
        running.index(label);
    }
    for at in 0..OPS {
        running.op(&[END]);
        match at {
            0 => {
                running.get(4).get(5).mem(I32_STORE, 0);
            }
            1 => {
                running.get(4).get(5).get(6).op(&[MISC]).index(11).op(&[0]);
            }
            2 => {
                running.get(4).get(5).get(6).op(&[MISC]).index(10).op(&[0, 0]);
            }
            _ => {
                let (_, _, params, results) = IMPORTS[at as usize - 3];
                for (arg, ty) in params.iter().enumerate() {
                    running.get(4 + arg as u32);
                    if *ty == I64 {
                        running.op(&[I64_EXTEND_I32_S]);
                    }
                }
                running.call(at - 3);
                for _ in results {
                    running.op(&[DROP]);
                }
            }
        }
        running.op(&[BR]).index(OPS - at);
    }
    running.op(&[END, BR]).index(0);
    running.op(&[END, END]);

    // `_initialize` and the expansion, of a local `n` after their parameters.
    let phase = |code: &mut Code, n: u32, in_initialize: bool| {
        code.call(load).tee(n).i32(1).op(&[I32_GE_S, IF, EMPTY]);
        code.i32(BUF).mem(I32_LOAD8_U, 0).i32(IN_INITIALIZE.into()).op(&[I32_AND]);
        if !in_initialize {
            code.op(&[I32_EQZ]);
        }
        code.op(&[IF, EMPTY]).get(n).call(run).op(&[END, END]);
    };
    let mut initializing = Code::default();
    let mut expanding = Code::default();
    let initialize_locals = match kind {
        Kind::Interpreter => {
            phase(&mut initializing, 0, true);
            phase(&mut expanding, 5, false);
            1
        }
        Kind::Echo => {
            expanding.call(load).tee(5).i32(0).op(&[I32_GE_S, IF, EMPTY]);
            expanding.i32(BUF).get(5).call(import("output")).op(&[END]);
            0
        }
    };
    let mut allocating = Code::default();
    allocating.i32(ALLOC_AT);
    let mut listing = Code::default();
    listing.i32(LIST_AT).i32(LIST.len() as i32).call(import("output"));

    let bodies = [
        loading.body(0),
        running.body(3 + ARGS as u32),
        initializing.body(initialize_locals),
        allocating.body(0),
        listing.body(0),
        expanding.body(1),
    ];
    let mut code = Vec::new();
    uleb(&mut code, bodies.len() as u64);
    for body in &bodies {
        uleb(&mut code, body.len() as u64);
        code.extend_from_slice(body);
    }
    section(&mut out, 10, &code);

    let mut initial = vec![0; LIST_AT as usize + LIST.len()];
    initial[IOV_AT as usize..][..4].copy_from_slice(&BUF.to_le_bytes());
    initial[IOV_AT as usize + 4..][..4].copy_from_slice(&CAP.to_le_bytes());
    initial[PATH_AT as usize..][..PATH.len()].copy_from_slice(PATH);
    initial[LIST_AT as usize..].copy_from_slice(LIST);
    let mut data = vec![1, 0x00, 0x41, 0x00, END];
    name(&mut data, &initial);
    section(&mut out, 11, &data);
    out
}

/// The site spans of an expansion: def, call and mixed, the host's span numbers 1, 2 and 3.
pub(crate) fn site(at: u32) -> Span {
    let file_id = EditionedFileId::new(FileId::from_raw(0), Edition::CURRENT);
    Span {
        range: TextRange::empty(TextSize::new(at)),
        anchor: SpanAnchor { file_id, ast_id: ROOT_ERASED_FILE_AST_ID },
        ctx: SyntaxContext::root(Edition::CURRENT),
    }
}

/// A module of `kind` compiled for this host, and its package.
pub(crate) struct Host {
    wasm: &'static Wasm,
    package: PathBuf,
    env: Env,
    /// One expansion at a time, as they share the package's `input`.
    turn: Mutex<()>,
}

impl Host {
    /// The host of the target `target`, in its temporary directory, cleared first.
    pub(crate) fn new(target: &str, kind: Kind) -> Host {
        let dir = std::env::temp_dir().join(format!("blaze-fuzz-{target}-{}", std::process::id()));
        _ = std::fs::remove_dir_all(&dir);
        let package = dir.join("package");
        std::fs::create_dir_all(&package)
            .unwrap_or_else(|e| panic!("{}: {e}", package.display()));
        assert!(package.is_absolute(), "the package is given by an absolute CARGO_MANIFEST_DIR");
        let path = dir.join("module.wasm");
        std::fs::write(&path, module(kind)).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        let wasm = Wasm::at(&path, None)
            .unwrap_or_else(|e| panic!("the {kind:?} module compiles and links: {e}"));
        let mut env = Env::default();
        env.set("CARGO_MANIFEST_DIR", package.to_string_lossy().into_owned());
        env.set("CARGO_PKG_NAME", "fuzz");
        Host { wasm, package, env, turn: Mutex::new(()) }
    }

    /// Where the module's package is on this machine.
    pub(crate) fn package(&self) -> &std::path::Path {
        &self.package
    }

    /// Expands the module's macro with `input` its package's `input`, as `phase` says: the
    /// most bytes the expansion asked for at once, and what it made.
    pub(crate) fn run(
        &self,
        input: &[u8],
        phase: u8,
    ) -> (usize, Result<tt::TopSubtree, ProcMacroExpansionError>) {
        let _turn = self.turn.lock().unwrap_or_else(|e| e.into_inner());
        let file = self.package.join("input");
        std::fs::write(&file, input).unwrap_or_else(|e| panic!("{}: {e}", file.display()));
        let kind = if phase & ATTR != 0 { ProcMacroKind::Attr } else { ProcMacroKind::Bang };
        let listed = Listed { kind, name: "fuzz".to_owned(), helpers: Vec::new() };
        let [def_site, call_site, mixed_site] = [0, 1, 2].map(site);
        let subtree = tt::TopSubtree::empty(tt::DelimSpan::from_single(call_site));
        let call = Call { subtree: &subtree, attrs: None, env: &self.env, def_site, call_site, mixed_site };
        blaze_fuzz::largest_allocation(|| {
            if phase & FRESH != 0 {
                return self.wasm.expand(0, &listed, None, &call).output;
            }
            let mut session = self.wasm.session(&self.env)?;
            session.expand(0, &listed, None, &call).output
        })
    }
}
