# Requests from lane link

The linker as written (`crates/blaze-link/`) programs against the items below. Each is what a
real link needs that step 0's interface does not carry; without it the link of any program
with a static, a `main`, a panic or a C library can't be made. The code is written against
exactly these shapes.

## 1. `blaze-codegen`: statics, unwind tables, line tables, weak externs

Where: `crates/blaze-codegen/src/lib.rs`.

Why: `Target::Static` names a static but nothing defines one (its bytes, alignment,
mutability, thread-locality, `#[link_section]`); a panic unwinds through call frame
information that no `CompiledFunction` carries; debug info needs code offsets to spans; std
refers to `__cxa_thread_atexit_impl` with `#[linkage = "extern_weak"]`, which must link to 0
when no library defines it.

Add to `CompiledFunction`, after `side`:

```rust
    /// How to unwind through it: none for a function that can't unwind (`nounwind`, wasm).
    pub unwind: Option<Unwind>,
    /// Its code offsets and the spans they are of, by `at`, for debug info. Spans are
    /// position-independent (anchor and index), so a reformat changes no function.
    pub lines: Vec<Line>,
```

and these items:

```rust
/// A code offset of a function and the span its instructions from there are of.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct Line {
    pub at: u32,
    pub span: SpanRef,
}

/// A function's unwind information, as its backend made it.
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Unwind {
    /// DWARF call frame information (ELF, Mach-O): the CIE's fields and initial instructions,
    /// and the FDE's instructions, as `.eh_frame` encodes them (DW_CFA_*).
    Dwarf {
        code_align: u32,
        data_align: i32,
        return_address: u16,
        cie: Vec<u8>,
        fde: Vec<u8>,
        /// The personality routine (`rust_eh_personality`) of a function with landing pads.
        personality: Option<Target>,
        /// Its language-specific data area: one of the function's `data`, by id.
        lsda: Option<ContentId>,
    },
    /// Windows x64: the `UNWIND_INFO` bytes without the handler, and the handler and its data.
    Windows { info: Vec<u8>, handler: Option<Target>, lsda: Option<ContentId> },
}

/// A static's definition. Its address is the linker's: by which static it is, never by
/// content (`Target::Static`).
#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub struct Static {
    pub item: ItemKey,
    pub instance: ContentId,
    /// A power of two.
    pub align: u32,
    pub init: Init,
    /// Written while the program runs: `static mut`, or a type with interior mutability.
    pub mutable: bool,
    pub thread_local: bool,
    /// `#[link_section]`, as written.
    pub section: Option<String>,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Init {
    Bytes { bytes: Vec<u8>, relocs: Vec<Reloc> },
    Zeroed { size: u64 },
}
```

and to `Target`:

```rust
    /// As [`Target::Extern`], linked weakly: 0 when nothing defines it
    /// (`#[linkage = "extern_weak"]`).
    Weak(String),
```

What the linker does with each relocation kind, so codegen emits the right one, is in
`docs/v2/plan/lanes/link/notes.md` ("Relocations codegen emits").

> applied: by codegen (`v2/requests/codegen.md` section 2: `unwind`, `lines`, `Line`, `Unwind`, `Static` with `data`, `Init`, `Target::Weak` in `crates/blaze-codegen/src/lib.rs`).

## 2. `blaze-link`: what a change carries, and the output's target

Where: `crates/blaze-link/src/lib.rs` (the interface items `Change` and `Image::new`). The
types named here (`Native`, `Output`) are new public items of blaze-link, already written.

Why: a link needs the statics (above), the names the program and C code observe
(`main`, `#[no_mangle]`, `#[export_name]`: a name is the item's, not the code's, so it can't
be in a `CompiledFunction`, which is shared by every item with the same code), the C objects,
archives and shared libraries of the link line (libc, a `cc`-built `.a`), and which target to
write. `Image::new(to)` has no target: a default (the host) would be a silent default.

`Change` gains:

```rust
    pub statics: Vec<&'a blaze_codegen::Static>,
    /// Statics no longer in the program, by `(item, instance)`.
    pub removed_statics: Vec<(ItemKey, ContentId)>,
    /// Names defined by the program's code: `main`, `#[no_mangle]`, `#[export_name]`, each
    /// a `Target::Function`, `Target::Direct` or `Target::Static`.
    pub names: Vec<(String, Target)>,
    pub unnamed: Vec<String>,
    /// The symbol name a backtrace and a debugger show for a function (its v0 mangled name,
    /// as rustc's symbol table has it); one with none shows as its address.
    pub debug_names: Vec<(InstanceKey, String)>,
    /// C objects, archives and shared libraries, in link-line order (`Native::order`).
    pub natives: Vec<Native<'a>>,
    pub removed_natives: Vec<ContentId>,
```

`Image::new` becomes `pub fn new(to: FileName, output: Output) -> Image`.

`crates/blaze-link/Cargo.toml` gains `blaze-items.workspace = true` (for `ItemKey`) and
`blaze-format.workspace = true` (for `SpanRef`), both in the workspace already; that file is
mine and already says so.

The span side tables of `Change::spans` are bytes in the format `blaze_link::SpanTable`
defines (`SpanTable::encode`, a new public item of blaze-link): per anchor, the file name as
the program observes it (`file!()`, `Location::file`) and each span's line and column. The
format lane owns spans; when it has its own side-table type, the link reads that instead and
`SpanTable` goes. Until then whoever fills `Change::spans` (the coordinator's wiring) encodes
with `SpanTable`.

> applied: by link in its own `crates/blaze-link/src/lib.rs` (`Change`'s fields; `Image::new(to, output)`, which returns a `Result`).

## 3. `blaze-core`: write at an offset, rename, file mode

Where: `crates/blaze-core/src/lib.rs` (`Effect`, `Done`) and the drivers.

Why: an incremental link writes the few pages that changed, not the whole executable
(a 500 MB debug binary is rewritten for a few kilobytes otherwise, well over the 10-30 ms
target), into a copy that shares the rest's blocks (`Effect::Clone`), then makes that copy
the executable atomically, so a program running from the old one keeps it and nothing sees
half a link. An executable needs its mode set: `Effect::Write` makes files 0644.

```rust
    /// `bytes` written at `at` of `file`, which exists; durable once a later
    /// [`Effect::Sync`] of the file completes. Completes with [`Done::Written`].
    WriteAt { file: FileName, at: u64, bytes: Vec<u8>, tag: Tag },
    /// `from` renamed over `to`, atomically: a reader sees the old `to` or the new.
    /// Completes with [`Done::Renamed`].
    Rename { from: FileName, to: FileName, tag: Tag },
    /// The file's permission bits set to `mode` (0o755 for an executable). Completes with
    /// [`Done::Moded`].
    Mode { file: FileName, mode: u32, tag: Tag },
```

`Done` gains `Renamed` and `Moded`. `Effect::tag` gains the three arms. The simulation keeps a
file's mode for `Stat`'s sake only.

> applied: by core (`v2/requests/core.md` item 4).

## 4. Workspace dependencies

Where: the root `Cargo.toml`, `[workspace.dependencies]`. `object` and `gimli` are in `Cargo.lock` already.

```toml
object = { version = "0.37", default-features = false, features = ["read_core", "std", "archive", "elf", "macho", "coff", "pe", "wasm"] }
gimli = { version = "0.33", default-features = false, features = ["read", "write", "std"] }
blaze-live = { path = "crates/blaze-live" }
```

Why: `object` reads the C objects, archives and shared libraries of a link line (ELF,
Mach-O, COFF, wasm), never writes one; `gimli` writes the DWARF of the debug side table.
`blaze-live` is the runtime a live program links; blaze-link writes the patches it reads, in
the format `blaze_live::patch` defines. `crates/blaze-link/Cargo.toml` uses all three with
`workspace = true`.

---
Routed or applied by the coordinator, 16:54.

> applied: by the coordinator; all three are in the root `[workspace.dependencies]`.

## 5. From speed-link-edge-macho: zero-copy native reads (contract, `model.rs`, `native.rs`)

Status: open. For lead `link` (window `link-next1`). Affects every reader (ELF, Mach-O, COFF,
wasm); speed-link-edge-elf hits the same wall.

What is slow today: every reader copies each section's bytes into `Data::Bytes(Vec<u8>)`
(`macho/input.rs:157`, `elf/read.rs:452`, `pe/coff.rs:404`, `wasm/object.rs:671`), and the core
copies them again (`native.rs:836`, `Cow::Owned(bytes.clone())`) though it already keeps the
input's bytes (`state.rs:119`, `Input::bytes: Arc<[u8]>`). Every symbol name is its own `String`
(`Symbol::name`, `SharedSymbol::name`): one allocation per symbol of every object, archive member
and `.tbd`/dylib (libSystem's stub alone has ~3,500 names per document). mold and wild read
sections and names in place from the mapped file.

The change (exact):
1. `model.rs`: `Data` gains `Input(Range<u64>)`: the section's bytes are `bytes[range]` of the
   slice handed to `Writer::read` (for an archive member, the member's slice). Readers use it for
   every section whose bytes are in the file unchanged; `Bytes` stays for bytes a reader makes.
   `Data::len` gets the arm `Data::Input(range) => range.end - range.start`.
2. `native.rs:836` borrows instead of cloning: `Data::Input(range) =>
   Cow::Borrowed(&input_bytes[range.start as usize..range.end as usize])` where `input_bytes` is
   `self.new.inputs[it.input].bytes` (or, for a member, its `entry.bytes` sub-slice), and
   `native.rs:401` / `:859` read `.eh_frame` the same way. `Cow<'a, [u8]>` needs the input's
   `Arc<[u8]>` to outlive the stage, which it does (`self.new.inputs`).
3. `model.rs`: `Object` and `Shared` gain `pub names: String` (every name of the file, one
   allocation), and `Symbol::name` / `SharedSymbol::name` become `pub name: Name` with
   `#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)] pub(crate) struct Name { pub at: u32, pub len: u32 }`
   and `impl Object { pub fn name(&self, name: Name) -> &str { &self.names[name.at as usize..][..name.len as usize] } }`
   (same on `Shared`). `native.rs:103`, `:261`, `:274`, `:301`, `:675` call `object.name(symbol.name)`.
   Mach-O strips the leading `_` and ELF names are already the core's, so most names are a
   range of the file's string table copied once into `names`; with item 1 a later step can make
   `names` a range of the input too.

Why it is faster: no allocation per section or per symbol on read (two for a whole object);
section bytes copied once, into the output, instead of three times. Until this is applied the
readers keep the current types; the reader-side edits are mechanical and listed in
`/tmp/blaze-lanes/v2-speed-link-edge-macho.md`.

## 6. From speed-link-edge-macho: a writer may encode pointer slots (Mach-O chained fixups)

Status: open, a proposal. For lead `link`.

What: Mach-O's `LC_DYLD_CHAINED_FIXUPS` (ld-prime's default from macOS 12) stores each rebase
and bind in the 8-byte slot itself: target (or import ordinal) plus the distance to the next
slot of the same page. Today the core writes every `Abs64` slot as the unslid address and the
writer can only add file parts, so it cannot emit chains.

The change: `Writer` gains
`fn encode(&self, placed: &Placed<'_>, region: usize, bytes: &mut [u8]) -> Result<Vec<Range<u64>>, LinkError>`
with a default body `Ok(Vec::new())`, called by `file.rs` after relocation and before
`synthesize` for every region holding a rebase or bind (in parallel, one call per region),
returning the byte ranges it rewrote so `Stage::dirty` writes them. Mach-O implements it for
deployment targets of macOS 12 and later (`Output::platform.minimum >= 0x000c_0000`);
`LC_DYLD_INFO_ONLY` opcodes stay for older ones, as ld64 chooses.

Why: chains are built in one pass per page as slots are placed (no sort when the slots are
already in address order), cost dyld nothing until a page is touched, and a relink rewrites only
the slots whose target or neighbour changed instead of re-encoding every opcode.

## 7. From speed-link-edge-elf: what the ELF writer's speed needs from the core

Status: open. For lead `link` (window `link-next1`). Notes:
`docs/v2/plan/lanes/link/speed-link-edge-elf.md`.

1. **Request 5 above, for ELF too.** The ELF side, once `Data::Input` and `Name` exist:
   `elf/read.rs` `section()` gives `Data::Input(sh_offset..sh_offset + sh_size)` for every
   non-`NOBITS` loaded section (the same range holds when the reader copied the bytes to align
   them: the copy starts at offset 0), and symbol names become ranges of `.strtab`. ELF has the
   same wall: today an object of N loaded sections and M symbols costs N `Vec<u8>` copies and M
   `String`s on read, and libc.so.6 alone ~3,000 `String`s.
2. **`SharedSymbol::version` as an index.** `model.rs`: `Shared` gains `pub versions: Vec<String>`
   and `SharedSymbol::version` becomes `Option<u16>` into it; the core's import resolution
   reads `shared.versions[it as usize]`. Why: libc.so.6's ~3,000 default-versioned symbols name
   ~40 versions; today each symbol carries its own copy (`elf/read/shared.rs`, and the Mach-O and
   PE readers have none or one). With the change the ELF reader validates each `Verdef` name once.
3. **Read natives in parallel.** `native.rs` `Stage::inputs` calls `self.writer.read` once per new
   native, in a loop; `Writer: Sync`, so read them with rayon first
   (`change.natives.par_iter().filter(new).map(|it| writer.read(..)).collect::<Vec<_>>()`), then
   push in link-line order, taking the first error in that order. Likewise `Stage::member`:
   members are pulled one at a time as resolution finds them; pulling every member one round of
   resolution needs, then reading them with `par_iter`, is how mold and wild parse (one task per
   input file). The readers are pure functions of their bytes.
4. **Two contract facts, written down in `model.rs`.** (a) `Writer::file` sees each synthetic
   region's bytes as `Writer::synthesize` returned them (`file.rs` `finish` does this today): the
   ELF writer now reads `.dynamic`, `.interp`, the build ID, `.eh_frame_hdr` and
   `.gnu.version_r` back from `Placed::contents` instead of building every table a second time.
   (b) `Placed::rebase`, `got`, `binds` and `functions` are in address order (`write.rs` sorts
   them). The ELF writer checks with `is_sorted` and sorts only if they are not, so (b) is only
   documentation; (a) is relied on.
5. **Incremental synthesis (proposal, not needed for correctness).** Every link reruns the ELF
   writer's tables whole. They now cost O(n) parallel work, but an incremental relink that moved
   one function should cost O(change): `Placed` could carry the previous link's synthetic bytes
   and what changed since (imports, exports and rebase lists as added/removed sets), so
   `.rela.dyn` (the biggest table) is patched in place and `.dynsym`/`.gnu.hash` are kept when
   no import or export changed.
6. **A fuzz entry for the native readers (for the `safety` lane's `fuzz/`).** `Writer::read` and
   `model::writer` are `pub(crate)`, so a fuzz target outside the crate can't reach the ELF
   reader. `lib.rs` could export
   `#[doc(hidden)] pub fn fuzz_read(format: Format, arch: Arch, kind: NativeKind, bytes: &[u8]) -> bool`
   (build the writer for a minimal `Output`, call `read`, return `is_ok()`; `NativeKind` is
   already public). Invariant for the target: `read` returns `Ok` or `Err` on any bytes and
   never panics. The ELF reader indexes only through checked lookups (`get`, bounds checked
   before `[]`) and `object`'s checked parsers, and holds no `unsafe`
   (`elf.rs` is `#![forbid(unsafe_code)]`).

### 5a. Follow-up to item 5 (from speed-link-edge-macho-input, via speed-link-edge-macho)

With the `names` arena of item 5, three more per-atom allocations go: `Section::name` becomes
a `Name` into the same arena (Mach-O splits one section into many atoms, each today a clone of
its `SEG,SECT` `String`); `Section::fixups` becomes `Range<u32>` into one
`Object::fixups: Vec<NativeFixup>` per object (CSR; the Mach-O reader already builds that column,
sorted by atom); `Archive::index` and `Member::name` become `Name`s into an `Archive::names`
arena. The core's reads (`native.rs`: `section.name`, `section.fixups`, `archive.index`) change
to `object.name(section.name)`, `&object.fixups[section.fixups]`, `archive.name(..)`.

## 8. `blaze-codegen`: the wasm function format and `Static::imports` (blocks blaze-link's build)

Status: open. For codegen (or the coordinator). blaze-link now uses both; it does not build
until both are in.

1. `crates/blaze-codegen/src/wasm.rs` and `pub mod wasm;` in its `lib.rs`: `ValType`,
   `Signature` (`encode`, `decode`), `Import { module, field, ty }`, `Code { ty, types,
   type_fixups, stack_pointer, imports, export }` with `Code::encode(&self, body) -> (Vec<u8>, u32)`
   and `Code::decode(code) -> Result<(Code, usize), String>`. The full text is in
   `v2/requests/link-pewasm-wasm.md` (request W1 of link-pewasm-wasm). The `code` of an
   `Arch::Wasm32` `CompiledFunction` is `Code::encode`'s output, its relocations counted from
   the start of it; this is what `hir-ty-codegen/wasm.rs`'s `WasmFunction::encode` already does.
2. `blaze_codegen::Static` gains `pub imports: Vec<(String, wasm::Import)>`: what each
   `Target::Extern` its relocations name is imported as on wasm (a function pointer to a
   `#[link(wasm_import_module)]` function in a static); empty elsewhere.

Used by: `blaze-link/src/model.rs` (`Symbol::import`, `Import::wasm`), `state.rs`
(`wasm_imports`), `stage.rs` (`Code::decode` of each wasm function, `Static::imports`),
`native.rs` (an undefined function with an import becomes one: rustc's `--allow-undefined`).

## 9. Coordinator and codegen: `Change::slots` for an edited function

Status: open. For whoever builds a `blaze_link::Change` from codegen's output.

An edited function gets a new `InstanceKey`, but its callers' relocations name the old one's
slot (`Target::Function(old)`). For an edit, pass the old key in `removed`, the new function in
`added`, and `(old, new)` in `Change::slots`; for a tier-up, codegen's `TieredUp::replaced`
pairs. Without it the edit links (the new function gets a slot of its own) but callers keep
calling the old code, or the link fails naming a caller of the removed function. See
`docs/v2/plan/lanes/link/notes.md`, "The jump table".

## Answers to items 5-7 (lead `link`, 2026-10-29)

- 5 (zero-copy `Data::Input`, `Name` arenas): accepted, not applied. It changes the type every
  reader builds (ELF, Mach-O, COFF, wasm), each owned by a running lane; it goes in as one change
  once those lanes are done, with `native.rs` borrowing the input's bytes.
- 6 (Mach-O chained fixups, `Writer::encode`): accepted as a proposal, not applied; it needs
  `Stage::dirty` to take the writer's ranges and the live patch to carry them.
- 7.1, 7.2: as 5. 7.3 applied: new natives are read in parallel (`native.rs` `inputs`); members
  are still pulled one round at a time. 7.4 applied: both facts are in `model.rs` (`Placed`,
  `Writer::file`). 7.5: not applied (a proposal). 7.6 applied: `blaze_link::fuzz_read`.
