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

> forwarded to link: `crates/blaze-link/` is lane link's.

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

> forwarded to link: `crates/blaze-link/` is lane link's.

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

> forwarded to link: `crates/blaze-link/` is lane link's.

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

> forwarded to codegen: `crates/blaze-codegen` is lane codegen's.

## 9. Coordinator and codegen: `Change::slots` for an edited function

Status: open. For whoever builds a `blaze_link::Change` from codegen's output.

An edited function gets a new `InstanceKey`, but its callers' relocations name the old one's
slot (`Target::Function(old)`). For an edit, pass the old key in `removed`, the new function in
`added`, and `(old, new)` in `Change::slots`; for a tier-up, codegen's `TieredUp::replaced`
pairs. Without it the edit links (the new function gets a slot of its own) but callers keep
calling the old code, or the link fails naming a caller of the removed function. See
`docs/v2/plan/lanes/link/notes.md`, "The jump table".

> forwarded to codegen: whoever builds the `Change` from codegen's output is codegen's live path. > forwarded to coordinator: the integration side, listed in the requests report.

## Answers to items 5-7 (lead `link`, 2026-10-29)

- 5 (zero-copy `Data::Input`, `Name` arenas): accepted, not applied. It changes the type every
  reader builds (ELF, Mach-O, COFF, wasm), each owned by a running lane; it goes in as one change
  once those lanes are done, with `native.rs` borrowing the input's bytes.
- 6 (Mach-O chained fixups, `Writer::encode`): accepted as a proposal, not applied; it needs
  `Stage::dirty` to take the writer's ranges and the live patch to carry them.
- 7.1, 7.2: as 5. 7.3 applied: new natives are read in parallel (`native.rs` `inputs`); members
  are still pulled one round at a time. 7.4 applied: both facts are in `model.rs` (`Placed`,
  `Writer::file`). 7.5: not applied (a proposal). 7.6 applied: `blaze_link::fuzz_read`.

> applied: noted; 5 to 7 are lane link's own.

## 10. From speed-link-edge-live: encode the live message once, and a shorter AArch64 stub

Status: open. For lead `link` (`crates/blaze-link/src/live.rs`, `commit.rs`, `table.rs`). The
`blaze-live` side is in place, and the bytes on the wire are unchanged. See
`docs/v2/plan/lanes/link/speed-link-edge-live.md`.

1. `live.rs` `message()`: each piece is copied (`self.new.bytes(piece).to_vec()`) into an owned
   `Patch`. `blaze_live::patch::PatchRef<'a>` has the same fields, borrowing: `code` and `data`
   are `&'a [(u64, &'a [u8])]`, and `rebase`, `cells`, `frames` and `windows` are slices. Build
   the `(address, &[u8])` list over `self.new.bytes(piece)` and call
   `PatchRef::encode_into(&mut out)`, which reserves `encoded_len()` and writes once.
   `Patch::with(|it| ..)` gives the same view of an owned `Patch`, for tests.
2. `commit.rs`: `Writes::message` is `Option<(u64, Message)>`, encoded in the step
   (`message.encode()`). Carrying the bytes encoded by item 1 (`Option<(u64, Vec<u8>)>`) makes
   one buffer, moved into the one `Effect::Write`, which already writes a temp file and renames
   it. `RollBack` and `Restart` take `Message::encode_into` the same way.
3. `table.rs` AArch64 stub (6 instructions now: `adrp`, `ldr` of the cell, `movz`, `movk`, `ldr
   x16, [x16, x17]`, `br`): while `slot * 8 < 32768`, `adrp x16, cell; ldr x16, [x16,
   :lo12:cell]; ldr x16, [x16, #slot*8]; br x16` (4). Up to 2^21 slots, add `add x16, x16,
   #(slot*8 >> 12), lsl #12` before the load (5). Pad it to 24 bytes. That is one register and
   two instructions fewer on every call; the loads stay address dependent, so the cell's release
   store still orders the slots.

> forwarded to link: lane link's paths.

## 7. From speed-link-edge-macho (found by speed-link-edge-macho-dylib): a name index per `Shared`

Status: open. For lead `link`.

What is slow: `native.rs:672-677` builds an `FxHashMap<&str, (usize, &SharedSymbol)>` over every
symbol of every shared library on every link (libSystem's stub alone is about 10k names) only to
look up the names the link imports.

The change: `model.rs` `Shared` gains `pub index: Vec<u32>` (open addressing, a power of two
long, at most half full, each slot an index into `symbols` or `u32::MAX`, slot
`FxBuildHasher.hash_one(name) as usize & (len - 1)`, probed linearly) and
`impl Shared { pub fn find(&self, hash: u64, name: &str) -> Option<&SharedSymbol> }`. `native.rs`
hashes each wanted name once and asks each library in order
(`libraries.iter().enumerate().find_map(|(at, (_, it))| it.find(hash, name).map(|s| (at, s)))`),
the same first-library-wins answer. The readers build it once per file: Mach-O's `tbd.rs`
already builds exactly this table to deduplicate (`Seen`), a 3-line move; `dylib.rs`, ELF and
COFF build it the same way.

Why: the work becomes proportional to the imports, not the exports, and is done once per parsed
input rather than per link.

> forwarded to link.

## From safety-audit

From `safety-audit-linkgen` (children `-live` for blaze-live). Every file below was BUSY (owners
`link`, `speed-link-edge-live`, `fastcompile-*`), so nothing was edited: these are comments,
`# Safety` sections and block splits only, no behavior change, unless a line says MAY BE UNSOUND.
Full reasoning: `/tmp/blaze-lanes/v2-safety-audit-linkgen.md` and `-linkgen-live.md`.
`blaze-link` is clean (11 blocks, each one op with a real SAFETY comment).

> forwarded to link: `blaze-splice`, `blaze-in-place`, `blaze-live` are its.

### blaze-splice
- `crates/blaze-splice/src/sha256.rs:83` block: 8 ops (`vld1q_u32` x3, `.add(4)` x2, `vld1q_u8` in the `load` closure, `vst1q_u32` x2). Split, in the same order (the value intrinsics `vrev32q_u8`, `vsha256*`, `vaddq_u32` are safe fns in the pinned stdarch and need no block):
  - `// SAFETY: \`state\` is 8 \`u32\`s, so its first 4 are the 16 readable bytes \`vld1q_u32\` reads.` then `let mut abcd = unsafe { vld1q_u32(state.as_ptr()) };`
  - `// SAFETY: \`state\` is 8 \`u32\`s, so 4 past its start is inside it.` then `let upper = unsafe { state.as_ptr().add(4) };`
  - `// SAFETY: \`upper\` is the last 4 of \`state\`'s 8 \`u32\`s, the 16 bytes \`vld1q_u32\` reads.` then `let mut efgh = unsafe { vld1q_u32(upper) };`
  - in `load`: `// SAFETY: \`at\` is 0, 16, 32 or 48 of a 64-byte block, so 16 bytes from it are in \`block\`; \`vld1q_u8\` reads them at any alignment.` then `let bytes = unsafe { vld1q_u8(block[at..].as_ptr()) }; vreinterpretq_u32_u8(vrev32q_u8(bytes))`
  - `// SAFETY: \`group < 16\`, so \`K[group * 4..]\` has at least the 4 \`u32\`s \`vld1q_u32\` reads.` then `let round = unsafe { vld1q_u32(K[group * 4..].as_ptr()) }; let mixed = vaddq_u32(words[at], round);`
  - `// SAFETY: \`state\` is 8 writable \`u32\`s; the first 4 are the 16 bytes \`vst1q_u32\` writes.` then `unsafe { vst1q_u32(state.as_mut_ptr(), abcd) };`
  - `// SAFETY: \`state\` is 8 \`u32\`s, so 4 past its start is inside it.` then `let upper = unsafe { state.as_mut_ptr().add(4) };`, then `// SAFETY: \`upper\` is the last 4 of \`state\`'s 8 \`u32\`s, the 16 bytes \`vst1q_u32\` writes.` then `unsafe { vst1q_u32(upper, efgh) };`

### blaze-in-place
- `crates/blaze-in-place/src/lib.rs:59` block: no SAFETY. Proposed: `// SAFETY: \`lseek\` only moves the offset of \`fd\`, which \`source\` keeps open for this whole function; were it closed, the call would fail with EBADF, not touch memory.`
- `crates/blaze-in-place/src/lib.rs:67` block: no SAFETY. Proposed: `// SAFETY: \`fd\` and \`out\` are \`source\`'s and \`target\`'s descriptors, open until this function returns; \`source\` and \`target\` are locals the kernel reads and advances, and \`len\` bounds no memory of ours.`

### blaze-live

`crates/blaze-live` doesn't take `[lints] workspace = true`; it was audited as if it did. The
numbers in "MAY BE UNSOUND (n)" below refer to this list:

1. Failed `apply` reuses live memory (`linux.rs` `apply` 155-250, `macos.rs` 193-265, `windows.rs`
   154-210). `process.next` only moves, and is only published, after every step succeeds. If a step
   fails after the patch's memory is mapped (`register_frames`, `first_values`, or a `self.write` in
   the undo or redirect loops), some jumps may already point into that memory, and on Linux some
   frames may already be registered. The daemon still sees the old `next`, so it lays out the next
   patch at the same `base`. That patch then maps over the memory (MAP_FIXED, FIXED|OVERWRITE, or
   MEM_COMMIT over committed pages), and any thread calling a redirected function runs the new
   bytes at the old offsets until the new jumps are written.
2. Jumps are written under running threads (`linux.rs:319`, `windows.rs:292`, `macos.rs:313` and
   `swap_pages`, and the same for putting originals back). On x86-64, `copy_nonoverlapping` of a
   5-byte `jmp rel32` is not one atomic store, and another core can fetch a half-written jump
   (Intel's cross-modifying-code rule). On every platform, a thread stopped inside the first N bytes
   of the old function resumes in the middle of the new jump. On AArch64 the far jump is 3
   instructions (`blaze-splice` `far_jump`, macho `Code::stub`: `adrp; ldr|add; br x16`), so a thread
   at entry+4 or +8 runs `br x16` with a garbage x16. The lib.rs doc says "a function that is
   running when its patch lands finishes as it was", but that holds only for the thread calling
   `poll`, and not at all under `apply_as_they_come`.
3. Original bytes are read at an unchecked address (`linux.rs:233`, `macos.rs:247`, `windows.rs:193`).
   `write.address` comes from the patch file and is never checked to lie in the executable's image,
   although `Image` knows the range (`start..end`, `base..base+len`). The comments ("the executable's,
   which is mapped") are not upheld. Also, the `&[u8]` over a static that other threads write is a
   data race.
4. macOS bounds math is unchecked (`macos.rs:195`, `:207`). `code.base + len.next_multiple_of(PAGE)`
   and `code.base + self.slide` use `+`, where `linux.rs` and `windows.rs` use `checked_add`. In
   release, a wrapped `end` passes `end > self.process.end`, and `mach_vm_allocate(FIXED | OVERWRITE)`
   then maps over whatever is at the wrapped address.
5. macOS registers an FDE that runs past its section (`macos.rs:594`). `register_frames` calls
   `__register_frame` before checking that `at + 4 + length <= section.len()`; `linux.rs:607` does
   check this. Apple's libunwind then parses past the patch's text.
6. Linux uses the wrong `__register_frame` convention (`linux.rs:615`). It registers each FDE
   separately, which is Apple's convention. On Linux, libgcc (and LLVM's libunwind on ELF) takes a
   run of entries ending in a zero length; `slots/` knows this (`Dwarf::Sections`). Each call
   registers from that FDE on to a zero terminator that the loop never requires inside the patch, so
   the unwinder can read past the section, and overlapping runs are registered once per FDE.
7. Unwind tables are never deregistered from memory that gets rewritten (`slots/linux.rs:266`,
   `slots/macos.rs:221`, `slots/windows.rs:266`). The switch lets a patch write code over a
   generation that is no longer kept (rolled back, dropped, or evicted past `KEPT`). The FDEs or
   RUNTIME_FUNCTIONs registered for that generation stay registered and now point at other bytes.
   There is no `__deregister_frame` or `RtlDeleteFunctionTable`. The comments say the memory "stays
   mapped", but the unwinder needs it to stay unchanged.
8. Code is written over a generation that may still be running (`slots/switch.rs:531-540`; the
   comments at `slots/linux.rs:224-226`, `slots/macos.rs:248-250` and `slots/windows.rs:208-210`).
   `apply` refuses only writes over the current lineage. A thread that entered a function of a
   generation before it was rolled back or evicted, and has not returned (a loop, a blocking call),
   runs bytes being overwritten. "No kept generation's code is there" does not mean no thread runs
   it, and `poll`'s "call where nothing is half done" covers only the calling thread.
9. Data writes race other threads (`slots/linux.rs:142-144`, `:237-240`; `slots/macos.rs:190-192`;
   `slots/windows.rs:208-211`, `:343-345`; legacy `write`). `Writes::put` requires that nothing else
   reads *or writes* the memory. The comments assert only "nothing else of the program's writes ...
   while `poll` writes it". Nothing enforces even that, since other threads keep running during
   `poll`, and nothing covers readers. A static that another thread uses during the write is a data
   race.
10. Data spans are never checked against the data region or the image (`slots/switch.rs:505-520`).
    The `Memory` doc says "the switch has checked each is where the descriptor allows", and the
    platform comments say "the image's data". In fact `apply` checks data spans only against the code
    region and the cell, so a span can land anywhere mapped: the heap, a stack, another library, or
    the loader's jump table, which `Switch::image` (`:649`) reads as `&[u64]` under "Nothing writes
    it". `layout.data` is never used for this.
11. Counters are read racily (`sample.rs:245`). `read_volatile` runs while other threads do
    non-atomic adds to `blaze_live_counters`. Volatile is not atomic, so this is a data race in the
    Rust memory model. (On AArch64 an aligned u64 read does not tear.)
12. The Windows legacy runtime does not check the function table's range (`windows.rs:271-273`).
    `register_functions` checks that `len` is whole entries and that `start` is 4-aligned, but not
    that `start + len` lies in the `len` bytes just mapped. `RtlAddFunctionTable` then reads past the
    patch. `linux.rs` and `macos.rs` bound the same range with `text.get(..)`.

#### crates/blaze-live (crate)
- `crates/blaze-live/Cargo.toml` crate: no `[lints] workspace = true`, so `unsafe_op_in_unsafe_fn`, `clippy::undocumented_unsafe_blocks` and `clippy::multiple_unsafe_ops_per_block` aren't denied here. Add `[lints]\nworkspace = true` once the findings below are in. The crate has unsafe in compiled code, so no `#![forbid(unsafe_code)]`.
- `crates/blaze-live/src/lib.rs:157` `#[unsafe(no_mangle)]`: not checked by the lint; proposed anyway: `// SAFETY: the \`blaze_live_\` prefix is this crate's, so no other item of a program exports \`blaze_live_counters\`; a second one would fail the link or bind the daemon's code to the wrong counters.`

#### crates/blaze-live/src/linux.rs
- `crates/blaze-live/src/linux.rs:170` block: no SAFETY. Proposed: `// SAFETY: MAP_FIXED maps fresh read-write pages over \`len\` bytes at the page-aligned \`mapped\` (\`code.base\` whole pages, checked), all inside the room \`reserve\` set aside (\`process.next..process.end\`, checked above): nothing below \`next\` (earlier patches' code, which threads may run) or above \`end\` (statics of \`first_values\`) is replaced, and no thread jumps here before the writes below; the room is never unmapped, so the patch lives as long as the process; if the range left the room, MAP_FIXED would replace live code or data.` (See MAY BE UNSOUND 1 for a failed earlier apply.)
- `crates/blaze-live/src/linux.rs:186` block: no SAFETY. Proposed: `// SAFETY: \`mapped..mapped + len\` was mapped read-write just above (checked to be at \`mapped\`), page-aligned so any \`u8\` alignment holds, and nothing else refers to it yet: no jump reaches it before the writes below; the slice is not written after the text is made read-and-run.`
- `crates/blaze-live/src/linux.rs:193` block: no SAFETY. Put it on the line between `if text_len != 0` and `&& unsafe {` (the `if` stops the lint's walk up). Proposed: `// SAFETY: \`text_len\` bytes (whole pages, checked) at the page-aligned start of the mapping made above; taking write away affects only \`memory\`, which writes nothing after this, and no thread runs the pages yet.`
- `crates/blaze-live/src/linux.rs:202` block: no SAFETY. Put it on the line between `if data_len != 0` and `&& unsafe {`. Proposed: `// SAFETY: \`data_len\` bytes at \`mapped + text_len\`, page-aligned since \`text_len\` is whole pages, of the mapping made above; they were read-write already.`
- `crates/blaze-live/src/linux.rs:233` block: MAY BE UNSOUND: `address` is `write.address + slide` from the patch file, never checked to lie in the executable's image (`Image::start..end`, not kept in `Live`), and the slice over a static that other threads write races (MAY BE UNSOUND 3). Proposed after a bounds check: `// SAFETY: \`address..address + bytes.len()\` is in the executable's image (checked against \`image\` above), mapped readable as long as the process runs; only this thread writes code there.`
- `crates/blaze-live/src/linux.rs:269` block: no SAFETY. Proposed: `// SAFETY: \`page\` is page-aligned, at or above this patch's \`end\` and below \`room_end\` (checked above), and not in \`lasting_pages\`, so MAP_FIXED replaces only untouched room that no patch's code or static uses; it is never unmapped, so later patches can refer to the static there for the life of the process.`
- `crates/blaze-live/src/linux.rs:293` block: no SAFETY. Proposed: `// SAFETY: \`address..address + bytes.len()\` is in pages mapped read-write above or by an earlier patch (\`lasting_pages\`); no code refers to this static before this patch's jumps are written below (a patch has every static it refers to, and none before gave this one), so no other thread reads it; \`bytes\` is a local, so they don't overlap; bytes need no alignment.`
- `crates/blaze-live/src/linux.rs:310` block: no SAFETY. Proposed: `// SAFETY: \`page..page + len\` covers \`address..address + bytes.len()\`, which a patch names in the executable (MAY BE UNSOUND 3: not checked here); adding write keeps read and run, so a thread running code on those pages keeps running; \`old\` gives the protection back below.`
- `crates/blaze-live/src/linux.rs:319` block: no SAFETY, and MAY BE UNSOUND (2): a byte copy over a function's entry is not atomic for another core fetching it, and a thread stopped inside the overwritten bytes resumes mid-jump. Proposed (states what does hold): `// SAFETY: the pages were made writable above; \`bytes\` is a local buffer, so they don't overlap, and a byte copy needs no alignment.` plus the cross-modifying-code invariant once there is one (all other threads stopped, or a single aligned atomic store of the jump).
- `crates/blaze-live/src/linux.rs:320` block: no SAFETY. Proposed: `// SAFETY: the same pages as above, given back the protection /proc/self/maps had at \`address\` (read-and-run when it had none); nothing holds a reference into them.`
- `crates/blaze-live/src/linux.rs:468` block: no SAFETY. Proposed: `// SAFETY: MAP_FIXED_NOREPLACE fails rather than replace anything mapped, and PROT_NONE makes the pages inaccessible, so this takes only address space nothing uses; the room is never unmapped, so patches mapped in it later live as long as the process.`
- `crates/blaze-live/src/linux.rs:493` block: no SAFETY. Proposed: `// SAFETY: signal 0 sends nothing and only asks whether \`pid\` exists; no memory is involved.`
- `crates/blaze-live/src/linux.rs:615` block: no SAFETY, and MAY BE UNSOUND (6, 1): this registers each FDE on its own (Apple's convention), but glibc's libgcc `__register_frame` takes a run of entries ending in a zero length; nothing here checks that one ends inside `section`, and runs overlap. Fix: register each run's start once, after checking for its zero terminator, as `slots/switch.rs` `frames` does for `Dwarf::Sections`. Proposed comment once it does: `// SAFETY: \`section[at..]\` is a run of CIEs and FDEs ending in a zero length inside the patch (checked above), in memory mapped and unchanged for the rest of the process; the unwinder reads it from now on.`

#### crates/blaze-live/src/macos.rs
- `crates/blaze-live/src/macos.rs:103` block: 2 ops (`_dyld_get_image_header`, `_dyld_get_image_vmaddr_slide`). Split: `let header = unsafe { _dyld_get_image_header(0) };` (SAFETY: `// SAFETY: it takes an index and returns null past the last image; image 0 is the executable, which dyld loaded before anything ran.`) then `let slide = unsafe { _dyld_get_image_vmaddr_slide(0) } as u64;` (SAFETY: `// SAFETY: it takes an index and returns 0 past the last image; image 0's slide stays the same for the life of the process.`).
- `crates/blaze-live/src/macos.rs:206` block: no SAFETY. Proposed: `// SAFETY: \`mach_task_self\` takes nothing and returns this task's port name, which needs no release; it can't fail.`
- `crates/blaze-live/src/macos.rs:209` block: 5 ops (`mach_vm_allocate`, `slice::from_raw_parts_mut`, `mach_vm_protect`, `sys_icache_invalidate`, `register_frames`), one comment for all. The comment also says nothing of lifetime, of what a failed earlier apply left (MAY BE UNSOUND 1), or of unchecked `+` (MAY BE UNSOUND 4). Split, in the same order: (a) `unsafe { check(mach_vm_allocate(task, &mut at, len.next_multiple_of(PAGE), FIXED | OVERWRITE)) }.map_err(|it| format!("mapping the patch: {it}"))?;` with `// SAFETY: OVERWRITE replaces whole pages at the page-aligned \`at\` (\`code.base\` whole pages, checked), inside the room \`reserve\` set aside, above every earlier patch (\`code.base >= process.next\`) and below the statics (\`end <= process.end\`): no code a thread may run and no static is replaced, and no jump reaches it before the writes below; the room is never deallocated, so the patch lives as long as the process.`; (b) `let memory = unsafe { std::slice::from_raw_parts_mut(at as *mut u8, len as usize) };` with `// SAFETY: \`at..at + len\` was just allocated read-write and zeroed, page-aligned so any \`u8\` alignment holds, and nothing else refers to it until jumps are written below.`; (c) inside `if !code.text.is_empty()`: `unsafe { check(mach_vm_protect(task, at, code.text.len() as u64, 0, READ | EXECUTE)) }.map_err(|it| format!("making the patch executable: {it}"))?;` with `// SAFETY: the text pages of the allocation above (whole pages, checked); no thread runs them yet and \`memory\` writes nothing there after this.`; (d) `unsafe { sys_icache_invalidate(at as *mut c_void, code.text.len()) };` with `// SAFETY: invalidates the instruction cache over the text just written and made runnable; it changes no memory.`; (e) `unsafe { register_frames(&memory[..code.text.len()], code.frames) }?;` with `// SAFETY: \`memory[..text.len()]\` is this patch's text, mapped for the life of the process and not written again (see \`register_frames\`).`
- `crates/blaze-live/src/macos.rs:195` and `:207`: MAY BE UNSOUND (4): `code.base + len.next_multiple_of(PAGE)` and `code.base + self.slide` are unchecked; in release a wrapped `end` passes the room check, and the `FIXED | OVERWRITE` allocation then lands outside the room. Use `checked_add`, as `linux.rs:159` and `windows.rs:159` do.
- `crates/blaze-live/src/macos.rs:247` block: MAY BE UNSOUND (3): the comment "what a patch writes over is the executable's, which is mapped" isn't upheld: `write.address` is never checked against `image.start..image.end` (passed only to the sampler), and the slice over a static that other threads write races. Proposed after a check: `// SAFETY: \`address..address + bytes.len()\` is in the executable's image (checked against \`image\` above), mapped readable as long as the process runs; only this thread writes code there.`
- `crates/blaze-live/src/macos.rs:270` block: no SAFETY. Proposed: `// SAFETY: \`mach_task_self\` takes nothing and returns this task's port name, which needs no release; it can't fail.`
- `crates/blaze-live/src/macos.rs:286` block: comment lacks lifetime and where it's upheld. Proposed: `// SAFETY: \`page\` is page-aligned, at or above this patch's \`end\` and below \`room_end\` (checked above), and not in \`lasting_pages\`, so OVERWRITE replaces only untouched room that no patch's code or static uses; it is never deallocated, so later patches may refer to the static there for the life of the process.`
- `crates/blaze-live/src/macos.rs:297` block: comment lacks why no other thread reads it. Proposed: `// SAFETY: \`address..address + len\` is in pages allocated read-write above or by an earlier patch; no code refers to this static before this patch's jumps are written below (a patch has every static it refers to, and none before gave this one), so no other thread reads it; \`bytes\` is a local, and a byte copy needs no alignment.`
- `crates/blaze-live/src/macos.rs:313` block: 2 ops (`ptr::copy_nonoverlapping`, `swap_pages`). Split, same control flow: `if protection & WRITE != 0 {` / `unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), address as *mut u8, bytes.len()) };` (SAFETY: `// SAFETY: \`address\` is mapped writable (\`protection_at\`); \`bytes\` is a local buffer, and a byte copy needs no alignment; a static another thread uses meanwhile races (MAY BE UNSOUND 9).`) / `return Ok(());` / `}` / `unsafe { swap_pages(address, bytes, protection) }` (SAFETY: `// SAFETY: \`address\` is mapped with \`protection\`, which isn't writable, so nothing writes those pages meanwhile; see \`swap_pages\`.`). Also, `protection_at` reads only the region at `address`, but a write running into the next region gets that region's pages copied and remapped with the first region's protection; check the whole range.
- `crates/blaze-live/src/macos.rs:345` block: 2 ops (`header.add`, `read_unaligned`). Split: `let u32_at = |at: usize| { let p = unsafe { header.add(at) } as *const u32; unsafe { p.read_unaligned() } };` with `// SAFETY: \`at\` is within the header and the load commands after it, which dyld mapped and checked against \`sizeofcmds\` before loading the image.` then `// SAFETY: \`p\` is 4 bytes in those mapped load commands; the read is unaligned, of a \`u32\` any bits make.`
- `crates/blaze-live/src/macos.rs:346` block: no SAFETY (the comment above 345 doesn't reach the next line), and 2 ops (`header.add`, `read_unaligned`). Split as for 345, with `*const u64` and `// SAFETY: \`p\` is 8 bytes in those mapped load commands; the read is unaligned, of a \`u64\` any bits make.`
- `crates/blaze-live/src/macos.rs:366` block: 2 ops (`header.add`, deref). Split: `let p = unsafe { header.add(at + 8 + nth) };` (SAFETY: `// SAFETY: an LC_UUID command is 24 bytes (dyld checked \`cmdsize\`), the last 16 the UUID, so \`at + 8 + nth\` for \`nth < 16\` is inside it.`) then `*byte = unsafe { *p };` (SAFETY: `// SAFETY: \`p\` is a byte of the mapped load commands.`).
- `crates/blaze-live/src/macos.rs:384` block: no SAFETY. Proposed: `// SAFETY: \`mach_task_self\` takes nothing and returns this task's port name, which needs no release; it can't fail.`
- `crates/blaze-live/src/macos.rs:390` block: no SAFETY. Proposed: `// SAFETY: \`address..address + ROOM\` is the allocation just made; taking all access away affects nothing else and keeps the room for patches, which map over it later.`
- `crates/blaze-live/src/macos.rs:515` block: no SAFETY. Proposed: `// SAFETY: \`mach_task_self\` takes nothing and returns this task's port name, which needs no release; it can't fail.`
- `crates/blaze-live/src/macos.rs:540` unsafe fn `swap_pages`: no `# Safety`. Proposed: `/// # Safety\n///\n/// \`address..address + bytes.len()\` lies in pages of this task mapped readable with \`protection\`, which isn't writable, and nothing writes or remaps them while this runs (the copy, then the remap, would lose such a write). A thread may run code on the pages while they are swapped: it sees the old pages or the new, so \`bytes\` must be valid code at every instruction boundary a thread can be stopped at (MAY BE UNSOUND 2).`
- `crates/blaze-live/src/macos.rs:543` block: no SAFETY. Proposed: `// SAFETY: \`mach_task_self\` takes nothing and returns this task's port name, which needs no release; it can't fail.`
- `crates/blaze-live/src/macos.rs:545` block: no SAFETY, and 7 ops (`mach_vm_allocate`, 2 × `ptr::copy_nonoverlapping`, and inside the closure `mach_vm_protect` and `mach_vm_remap`, then `mach_vm_deallocate` and `sys_icache_invalidate`). Split, same order: `check(unsafe { mach_vm_allocate(task, &mut scratch, len, ANYWHERE) })?;` (`// SAFETY: ANYWHERE takes no address in use; the new address goes to \`scratch\`.`); `unsafe { std::ptr::copy_nonoverlapping(page as *const u8, scratch as *mut u8, len as usize) };` (`// SAFETY: \`page..page + len\` is mapped readable and not written meanwhile (the caller's); \`scratch\` is \`len\` new bytes, so they don't overlap.`); `unsafe { std::ptr::copy_nonoverlapping(bytes.as_ptr(), (scratch + (address - page)) as *mut u8, bytes.len()) };` (`// SAFETY: \`address - page + bytes.len() <= len\`, so the write is inside \`scratch\`; \`bytes\` is the caller's buffer, not \`scratch\`.`); in the closure `check(unsafe { mach_vm_protect(task, scratch, len, 0, protection) })?;` (`// SAFETY: \`scratch\` is the allocation above, \`len\` bytes, which only this function refers to.`) and `check(unsafe { mach_vm_remap(task, &mut target, len, 0, FIXED | OVERWRITE, task, scratch, 0, &mut current, &mut max, INHERIT_NONE) })` (`// SAFETY: maps the copy over \`page..page + len\` (whole pages) in one call, so a thread running or reading them sees the old pages or the new, never a mix within a page, with the protection they had.`); then `unsafe { mach_vm_deallocate(task, scratch, len) };` (`// SAFETY: \`scratch\` is the allocation above, which nothing points into now; the remap holds its own reference to the pages.`); `swapped.map_err(...)?;`; inside `if protection & EXECUTE != 0`, `unsafe { sys_icache_invalidate(address as *mut c_void, bytes.len()) };` (`// SAFETY: invalidates the instruction cache over the bytes just written; it changes no memory.`).
- `crates/blaze-live/src/macos.rs:581` unsafe fn `register_frames`: no `# Safety`. Proposed: `/// # Safety\n///\n/// \`text\` is the patch's code, mapped and unchanged for the rest of the process (the unwinder keeps pointers into it), and \`frames\` names well-formed CIEs and FDEs inside it, each FDE's CIE pointer inside \`text\`.`
- `crates/blaze-live/src/macos.rs:594` block: MAY BE UNSOUND (5, 1): `__register_frame` is called before the loop checks that `at + 4 + length <= section.len()` (as `linux.rs:607` does), so an FDE running past the text is registered and parsed past it; and "which stays mapped" is not enough: after a failed apply (MAY BE UNSOUND 1) the next patch maps over it. Proposed once the bound is checked first: `// SAFETY: \`section[at..at + 4 + length]\` is one FDE inside the patch's text (checked above), which is mapped and unchanged for the rest of the process (the caller's); Apple's libunwind takes one FDE and keeps a pointer to it.`

#### crates/blaze-live/src/sample.rs
- `crates/blaze-live/src/sample.rs:193` block: 2 ops (`mach_absolute_time`, `mach_wait_until`). Split, keeping the comment about falling behind: `let now = unsafe { mach_absolute_time() };` (`// SAFETY: a clock read; it takes nothing and changes nothing.`), the `if next < now` block, then `unsafe { mach_wait_until(next) };` (`// SAFETY: waits until \`next\`, returning at once for a time that has passed; it touches no memory.`).
- `crates/blaze-live/src/sample.rs:204` block: 2 ops (`mach_task_self`, `mach_port_deallocate`). Split: `let task = unsafe { mach_task_self() };` (`// SAFETY: \`mach_task_self\` takes nothing and returns this task's port name, which needs no release.`) then `unsafe { mach_port_deallocate(task, me) };` (`// SAFETY: \`me\` is the right \`mach_thread_self\` gave at the start of \`run\`, given back once, here.`).
- `crates/blaze-live/src/sample.rs:245` block: 2 ops (`start.add`, `read_volatile`), and MAY BE UNSOUND (11): other threads add to the counters non-atomically while this reads with `read_volatile`, which is not atomic: a data race in the Rust memory model. Split: `.map(|at| { let counter = unsafe { start.add(at) }; (at as u32, unsafe { counter.read_volatile() }) })` with `// SAFETY: \`at < used <= COUNTERS\`, the length of the static array \`start\` points at, so the offset stays inside it.` and `// SAFETY: \`counter\` is an aligned \`u64\` of that static, valid for the life of the program.` The race needs a decision: `AtomicU64::from_ptr(counter).load(Relaxed)`, with the daemon's code adding atomically.
- `crates/blaze-live/src/sample.rs:278` block: 4 ops (`mach_task_self`, `task_threads`, `slice::from_raw_parts`, `vm_deallocate`). Split, same order, same early return: `let task = unsafe { mach_task_self() };` (`// SAFETY: \`mach_task_self\` takes nothing and returns this task's port name, which needs no release.`); `if unsafe { task_threads(task, &mut list, &mut count) } != 0 { return; }` (`// SAFETY: \`list\` and \`count\` are what the call fills in: an array the kernel allocates in this task, and its length.`); `let ports = unsafe { std::slice::from_raw_parts(list, count as usize) }.to_vec();` (`// SAFETY: the kernel gave \`count\` ports at \`list\`, page-aligned memory of this task; they are copied out before it is given back.`); `unsafe { vm_deallocate(task, list as usize, count as usize * size_of::<u32>()) };` (`// SAFETY: \`list\` is the kernel's array of \`count\` ports, copied out above and not used after; the rights to the ports are given back by \`give_back\`.`). Keep the existing comment's last clause on the last one.
- `crates/blaze-live/src/sample.rs:301` block: the comment above it ("each port is one `task_threads` gave a right to") is about the `mach_port_deallocate` at 303, not about `mach_task_self`. Proposed for 301: `// SAFETY: \`mach_task_self\` takes nothing and returns this task's port name, which needs no release.`
- `crates/blaze-live/src/sample.rs:303` block: no SAFETY. Proposed (moved from 301): `// SAFETY: each port is one \`task_threads\` gave a right to, given back once: \`drain\` takes it out of \`all\`.`

#### crates/blaze-live/src/windows.rs
- `crates/blaze-live/src/windows.rs:193` block: MAY BE UNSOUND (3): `write.address` is never checked to lie in `loaded..loaded + image.len`, so "the bytes are of the executable's image" isn't upheld; a static that other threads write races. Proposed after a check: `// SAFETY: \`address..address + bytes.len()\` is in the executable's image (checked against \`loaded..loaded + len\` above), loaded as long as the process runs; only this thread writes code there.`
- `crates/blaze-live/src/windows.rs:217` block: comment lacks lifetime and where it's upheld. Proposed: `// SAFETY: commits \`len\` bytes at the page-aligned \`mapped\` (checked), in the room \`reserve\` reserved, within \`process.next..process.end\` (checked by \`apply\`), so no earlier patch's code a thread may run is in them; the room is never released, so the patch lives as long as the process.` (See MAY BE UNSOUND 1: committing over a failed patch's pages keeps their bytes, which jumps may already reach.)
- `crates/blaze-live/src/windows.rs:227` block: comment lacks alignment and who else can see it. Proposed: `// SAFETY: the pages were just committed read-write at \`mapped\`, page-aligned so any \`u8\` alignment holds; no thread reaches them before the jumps \`apply\` writes after \`map\` returns, and the slice doesn't outlive \`map\`.`
- `crates/blaze-live/src/windows.rs:236` block: comment lacks who else can see it. Proposed: `// SAFETY: \`text_len\` bytes (whole pages, checked) at the page-aligned start of the pages committed above; \`memory\` writes nothing there after this, and no thread runs them yet.`
- `crates/blaze-live/src/windows.rs:251` block: 2 ops (`GetCurrentProcess`, `FlushInstructionCache`). Split: `let process = unsafe { GetCurrentProcess() };` (`// SAFETY: it takes nothing and returns this process's pseudo-handle, which needs no closing.`) then `unsafe { FlushInstructionCache(process, mapped as *const c_void, text_len as usize) };` (`// SAFETY: flushes the instruction cache over the code just written; it changes no memory.`).
- `crates/blaze-live/src/windows.rs:273` block: MAY BE UNSOUND (12, 7): `start + len` is never checked to lie in the `len` bytes just mapped, so `RtlAddFunctionTable` can read past the patch; and the table is never deleted when a failed apply's pages are reused (MAY BE UNSOUND 1). Proposed once bounded: `// SAFETY: \`len / FUNCTION\` RUNTIME_FUNCTIONs at the 4-aligned \`mapped + start\`, inside the patch's pages (checked above), which are never released or rewritten; their addresses are relative to \`loaded\`.`
- `crates/blaze-live/src/windows.rs:282` block: comment unchecked (MAY BE UNSOUND 3). Proposed: `// SAFETY: \`address..address + bytes.len()\` is in the executable's image (once checked, MAY BE UNSOUND 3); PAGE_EXECUTE_READWRITE keeps read and run, so a thread running code on those pages keeps running; \`old\` is given back below.`
- `crates/blaze-live/src/windows.rs:292` block: MAY BE UNSOUND (2): a byte copy of a 5-byte `jmp` over a function's entry is not atomic for another core fetching it, and a thread stopped inside it resumes mid-jump. What holds: `// SAFETY: the bytes were made writable above; \`bytes\` is a local buffer, so they don't overlap, and a byte copy needs no alignment.`, plus the cross-modifying-code invariant once there is one.
- `crates/blaze-live/src/windows.rs:295` block: "as above" names nothing. Proposed: `// SAFETY: the same bytes as above, given back the protection \`old\` that VirtualProtect said they had; nothing holds a reference into them.`
- `crates/blaze-live/src/windows.rs:302` block: 2 ops (`GetCurrentProcess`, `FlushInstructionCache`). Split as for 251: `let process = unsafe { GetCurrentProcess() };` (`// SAFETY: it takes nothing and returns this process's pseudo-handle, which needs no closing.`) then `unsafe { FlushInstructionCache(process, address as *const c_void, bytes.len()) };` (`// SAFETY: flushes the instruction cache over the bytes just written; it changes no memory.`).

#### crates/blaze-live/src/slots/ (owner is editing these now; lines as of 18:40 PT)
- `crates/blaze-live/src/slots/unix.rs:74` block (`(!path.is_null()).then(|| unsafe { CStr::from_ptr(path) })`): the SAFETY comment is two lines up, above `let path =`. The lint should walk up through the closure and method calls to the `let`, but to be sure, put the comment on the line between `let path =` and `(!path.is_null())`. Text unchanged.
- `crates/blaze-live/src/slots/switch.rs:531` (`apply`'s lineage check) and the comments at `slots/linux.rs:224-226`, `slots/macos.rs:248-250` and `slots/windows.rs:208-210`: MAY BE UNSOUND (8): only the current lineage's code is protected from being overwritten. A thread still inside code of a generation rolled back, dropped or evicted past `KEPT` runs bytes being rewritten. "No kept generation's code is there, so no thread runs them" does not follow. It needs a rule (never reuse code of a generation once published, or wait until no thread can be in it) and the comments should name it.
- `crates/blaze-live/src/slots/linux.rs:266`, `slots/macos.rs:221`, `slots/windows.rs:266`: MAY BE UNSOUND (7): FDEs and RUNTIME_FUNCTIONs are never deregistered, yet the switch lets a later patch write over the code region they are in (a generation no longer kept). The unwinder then parses whatever bytes are there. "Which stays mapped" must become "which stays unchanged", with `__deregister_frame` or `RtlDeleteFunctionTable` before reuse, or no reuse.
- `crates/blaze-live/src/slots/linux.rs:142-144`, `:237-240`; `slots/macos.rs:190-192`; `slots/windows.rs:208-211`, `:343-345`: MAY BE UNSOUND (9): `Writes::put`'s `# Safety` asks that nothing else reads or writes the memory. These comments claim only that nothing else writes the image's data while `poll` writes it, which nothing enforces (other threads keep running during `poll`), and they say nothing of readers. A static another thread uses during the write is a data race. Name the rule the program must keep (for example, other threads touch no static a patch rewrites while it is applied), or stop them.
- `crates/blaze-live/src/slots/switch.rs:505-520` (data spans) with the `Memory` trait doc at `:244-245` and `Switch::image` at `:646-649`: MAY BE UNSOUND (10): data spans are checked only against the code region and the cell, not against `layout.data` or the image, so "the switch has checked each is where the descriptor allows", the platforms' "the image's data", and `image()`'s "Nothing writes it" (the loader's table) aren't upheld. Check data spans against `layout.data` (and the loader's table), or reword the comments to say the linker is trusted for it.
- `crates/blaze-live/src/slots/tests.rs:69` block: "as for code" leans on "the switch checked it is in the region the descriptor names", which isn't true for data (see above). Proposed: `// SAFETY: every data write the tests make is to a buffer of their \`Program\`, which outlives the switch; the switch checks only that it misses the code region and the cell.`

## Re-check after speed-link-edge-live's note (18:25 PT)

`speed-link-edge-live` says `slots/` now follows `docs/v2/design/unsafe.md`: the message is read,
not mapped, and every block has one op and a SAFETY comment (details in
`docs/v2/plan/lanes/link/speed-link-edge-live.md`, section "Unsafe"). A rescan agrees: in `slots/`,
the three lints leave only `slots/unix.rs:74`, where the comment's placement is worth a check (two
lines above the block). The scan's other `slots/` hits are its false positives on `extern` blocks.
The `slots/` soundness findings still stand on the current code:
- MAY BE UNSOUND 7: nothing deregisters frames or function tables.
- MAY BE UNSOUND 8: `switch.rs:530` still protects only the lineage.
- MAY BE UNSOUND 9: the comments at `slots/linux.rs:142` and `:238`, `slots/macos.rs:190` and
  `slots/windows.rs:210` still claim only that nothing else writes.
- MAY BE UNSOUND 10: data spans are still not checked against `layout.data`.

Their note also confirms that the older modules (`linux.rs`, `macos.rs`, `windows.rs`, `sample.rs`,
`lib.rs`) were kept as they were, so every finding on those stands. Every file is still BUSY under
`owners.py` (`link` BLOCKED, `fastcompile-*` with no report), so I still edited nothing.

> forwarded to link.

## 11. From speed-link-edge-macho (found by speed-link-edge-macho-input2): `Class::Named` allocates per section

Status: open, does not block. For lead `link` (`crates/blaze-link/src/model.rs`).

What is slow: `Class::Named { name: String, .. }`. The Mach-O reader makes one per Mach-O section
that is `__LD,__compact_unwind` (every C object has one: `macho.rs` `compact_unwind()`),
`__DATA,__thread_init` or a `#[link_section]` one (`macho.rs` `classify`, `format!` at ~271), and
clones it into every other atom of a section split into atoms. ELF and COFF named sections pay
the same. `Class` is also compared and hashed as a key by the core (`Writer::order`, region
lookup), each time comparing the `String`.

The change: `Class::Named`'s name interned once per link: a `u32` into a link-wide table of
section names (or `Arc<str>` as a first step, so the clone per atom is a count bump), with
`Writer::section`/`classify` returning the interned id. Fixed names (`__LD,__compact_unwind`,
`__DATA,__thread_init`) become constants of that table.

Why: no allocation per named section or per atom of one, and class comparisons on the core's
sorting and lookups become one `u32` compare.

> forwarded to link: its own `crates/blaze-link/`.

## 12. From speed-link-edge-live: the safety audit of blaze-live, answered, and what the linker owes

Status: open. Items a and b block jump-table patches after a rollback, and ELF patches with
frames from the second one on; items c and d don't block. For lead `link` (`blaze-link` place
and live writer, `blaze-splice`, `blaze-engine`'s counter code). Applied in `crates/blaze-live`
by speed-link-edge-live, unbuilt (this lane may not build); details in
`docs/v2/plan/lanes/link/speed-link-edge-live.md`, section "Safety audit".

What was applied for "## From safety-audit" / blaze-live:
- Every comment and split the audit proposed, in `lib.rs`, `linux.rs`, `macos.rs`, `windows.rs`,
  `sample.rs`, `slots/unix.rs:74` and `slots/tests.rs:69`. Each unsafe block now has one
  operation and a SAFETY comment. Where v1 can't uphold an invariant, the comment says "NOT upheld"
  and names the rule the program must keep.
- MAY BE UNSOUND 1: a failed apply now moves `next` past the patch and publishes it, so no later
  patch maps over memory that jumps or the unwinder may already reach. This is on all three v1
  platforms.
- MAY BE UNSOUND 3: each write is checked to lie in the executable's image before anything is
  mapped or written. The patch is declined otherwise.
- MAY BE UNSOUND 4 (`checked_add` on macOS), 5 (each FDE bounded before it is registered) and 12
  (the Windows function table must lie in the patch) are fixed.
- MAY BE UNSOUND 6: Linux now registers the `.eh_frame` run once, after finding its zero length
  inside the section or in the 4 bytes right after it in the patch's text. Without one, the patch
  is declined.
- MAY BE UNSOUND 7 and 8 (`slots/`): the switch keeps a set of lasting memory and refuses any
  write over it. The set covers the code of every generation that was registered or published,
  every registered FDE, each run's zero terminator, and Windows UNWIND_INFO (now sized and
  bounded).
- MAY BE UNSOUND 10 (`slots/`): data over the loader's jump table is refused. The platforms check
  that each data span lies in the image (unix `dladdr` base, Windows allocation base) and refuse
  pages with no access.
- MAY BE UNSOUND 11: the counters are a `static [AtomicU64; COUNTERS]`, and the sampler loads them
  relaxed with no unsafe.
- MAY BE UNSOUND 2 and 9 are stated as the program's rule, in the `lib.rs` and `slots` `poll` docs
  and in the "NOT upheld" comments. Finding 2 is v1's design and can't be fixed by a comment: the
  fix is to move v1 onto the jump table.

What the linker owes for these to hold without declining good patches:
a. Patch code placement must never go back. `place.rs` reuses addresses after a rollback because
   rollback restores the linker state. The switch now declines any code over a published
   generation, since a thread may still be in it. Keep the patch-region cursor monotonic across
   rollbacks, and never reuse an address that published code had.
b. Each ELF patch's EhFrame run must end in its own zero terminator, placed after the previous
   run's. Today each patch writes its run over the previous one's terminator, and the switch now
   declines that, because the unwinder may be reading the terminator. For v1 ELF (`blaze-splice`),
   append a zero length after the patch's `.eh_frame` in its text. A relocatable object's section
   has none, and the runtime declines the patch unless the next 4 bytes happen to be zero.
c. MAY BE UNSOUND 9: data a running program can reach must not be rewritten in place. Give a
   changed static a fresh piece and redirect to it, or restart. If in-place data writes stay,
   `Live::poll` should become `unsafe fn` with the rule in its `# Safety` (other threads touch no
   static a patch rewrites while it runs).
d. MAY BE UNSOUND 11 and 2, for blaze-engine and blaze-splice:
   - The daemon's counter code must add to `blaze_live_counters` with a relaxed atomic add
     (`ldadd` / `lock xadd`). Otherwise its adds still race the sampler.
   - v1's jumps should move to the `slots` jump table. Short of that, write each jump as one
     aligned atomic store, which a thread already inside the bytes can still resume into, so
     stopping threads is the only full fix.

Once these are in, `crates/blaze-live/Cargo.toml` can take `[lints] workspace = true`.

> forwarded to link.

## From safety-fuzz-link

From `safety-fuzz-link` (child of `safety-fuzz`, lane `safety`), who wrote the fuzz targets of
blaze-link's input readers: `fuzz/targets/link_{elf_object,elf_archive,elf_shared,macho_object,
macho_archive,macho_dylib,macho_tbd,coff_object,coff_archive,coff_import,wasm_object,
wasm_archive}.rs`, each against `fuzz_read` (item 7.6). Full notes:
`/tmp/blaze-lanes/v2-safety-fuzz-link.md`. Nothing in `crates/` was edited.

> forwarded to link.

### 12. A fuzz entry that returns what a read made (`lib.rs`)

Status: open, does not block (the targets run against `fuzz_read` now and check what a `bool`
allows). For lead `link`.

`fuzz_read` answers only whether a read succeeded, so a target can't check that what an accepted
read made is inside its input and consistent. Today the targets check it only indirectly: an
accepted ELF, Mach-O or COFF file must still be accepted with 64 bytes appended. Proposed, next
to `fuzz_read` in `src/lib.rs`, both `#[doc(hidden)]`:

```rust
/// What a read made, as ranges and indices (for a fuzz target).
#[doc(hidden)]
pub enum FuzzRead {
    Object {
        /// Per section: `Data::len`, and for `Data::Input` its range of the bytes read.
        sections: Vec<(u64, Option<std::ops::Range<u64>>)>,
        /// Per `SymbolDef::Section` symbol: (section, value, size).
        symbols: Vec<(u32, u64, u64)>,
        /// `Object::symbols.len()`.
        symbol_count: u32,
        /// Per fixup: (section, at, `reloc::width(apply)`, symbol, minus).
        fixups: Vec<(u32, u64, u8, u32, Option<u32>)>,
        /// Per group: its sections (`Object::grouped[group.sections]`).
        groups: Vec<Vec<u32>>,
    },
    Archive { members: Vec<std::ops::Range<usize>>, index: Vec<u32> },
    Shared { symbols: usize, more: usize },
}

/// [`fuzz_read`], with what the read made; `None` when it fails.
#[doc(hidden)]
pub fn fuzz_read_parts(format: Format, arch: blaze_codegen::Arch, kind: NativeKind, bytes: &[u8]) -> Option<FuzzRead>;
```

With it, every target checks these, the invariant the readers' contract states:

- every archive member's range is within `bytes.len()`;
- every index entry is below the member count;
- every `Data::Input` range is within `bytes.len()` and as long as the section's `len`;
- every symbol's section is below the section count, and `value + size` (no overflow) is at
  most that section's `len`;
- every fixup's section is below the section count, its `at + width` is at most that
  section's `len`, and its symbol and minus are below `symbol_count`;
- every group's sections are below the section count.

The PE and wasm archive bugs (1 and 2 below) break the first of these. Bug 2 doesn't panic
today, so a `bool` can't show it.

### Bugs found by reading

Each one's input is in the corpus or generated by the named target's `seeds()`, so that
target's `seeds` test fails until the bug is fixed.

1. **PE: an archive member past the archive's end panics** (`pe/coff.rs` `archive`, ~291).
   `Member::bytes` is `offset..offset + size` as `object` parses the header, and `object` does
   not bound a member's size by the archive's length (`ArchiveMember::parse` only skips it).
   Then `import_library` (~361) slices `&bytes[it.bytes.clone()]`, and so does
   `descriptor_dll`'s caller (~306): both panic. Read as `NativeKind::Shared`, it is 68 bytes:
   `!<arch>\n`, then one header `a.o/` of size 100 and no data
   (`fuzz/corpus/link_coff_import/member-past-the-end.hex`):
   `213c617263683e0a 612e6f2f202020202020202020202020 302020202020202020202020 302020202020
   302020202020 3634342020202020 31303020202020202020 600a`.
   As `NativeKind::Archive`, a GNU index naming `__IMPORT_DESCRIPTOR_a` in that member panics
   in `descriptor_dll`'s slice (`fuzz/corpus/link_coff_archive/descriptor-member-past-the-end.hex`).
   Fix: the ELF and Mach-O readers' check, `.filter(|it| it.end <= bytes.len())` with an error
   naming the member (`elf/read/archive.rs` 38-44).
2. **wasm: the same unchecked member range** (`wasm/object.rs` `archive`, 100-105). It doesn't
   panic in the reader (the core reads members with `.get`), but an accepted archive has a
   member outside its input: the same 68 bytes, read as `Archive { whole: true }`, are accepted
   (`fuzz/corpus/link_wasm_archive/member-past-the-end.hex`). Same fix.
3. **PE: a long import's name offset overflows** (`pe/coff.rs` `long_import`, ~1143).
   `target.value() as usize + u64_at(thunk, 0) as usize` adds two input values unchecked, so a
   thunk of `u64::MAX` and a name symbol at 1 panic on the overflow. `u16_at`/`u32_at`/`u64_at`
   (166-176) also compute `at + 2`/`+ 4`/`+ 8` unchecked, which overflows for an `at` near
   `usize::MAX`. Input: `thunk_past_usize()` in `fuzz/targets/link_coff_import.rs`, read as
   `Shared`: an import library whose GNU index names `__IMPORT_DESCRIPTOR_foo` in member 0 (a
   COFF object, `.idata$2` defining it and `.idata$6` `foo.dll`), and member 1 a long import
   (`.idata$5` holding the 8 bytes `ff`×8 and defining `__imp_f`, an undefined
   `__IMPORT_DESCRIPTOR_foo`, and an `ADDR32NB` at `.idata$5`+0 to a static symbol of value 1 in
   `.idata$6`). Fix: `checked_add`, refusing on overflow, in `long_import` and the three
   helpers (`bytes.get(at..at.checked_add(2)?)`).
4. **Mach-O: a section's address plus an offset overflows** (`macho/input.rs` 666 and 935).
   `self.inputs[s].addr + at` takes `addr` from the section header unchecked: a section at
   `0xffff_ffff_ffff_fff8` with an x86-64 non-extern `SIGNED`/`BRANCH` relocation at offset 8
   (666), or an `__eh_frame` FDE with a pc-relative pointer and no relocation (935), panics.
   Fix: `wrapping_add` as the rest of each expression does (the address is only matched against
   symbols), or refuse a section whose `addr + size` overflows when the inputs are read (~131).
5. **Mach-O: an archive's name arena grows quadratically with its index** (`macho/archive.rs`
   31-35). `index_bytes` sums every index entry's name length, but a BSD `__.SYMDEF` entry
   names its string by offset, so every entry can name the same one. 640 entries naming one
   4096-byte string make a 10 KB archive whose read asks for a 2.6 MB arena at once, past
   `blaze_fuzz::allowed` (1.7 MB). At 1 MB the arena is about 32 GB. Input:
   `one_name_many_times()` in `fuzz/targets/link_macho_archive.rs`. Fix: one `Name` per
   distinct string offset (BSD) instead of one push per entry, or bound `index_bytes` by the
   index's size and push names past it as they come.
6. **wasm: the same arena** (`wasm/object.rs` `archive`, 120): one `names.push` per index
   entry, and the reader takes BSD archives too. Input: `one_name_many_times()` in
   `fuzz/targets/link_wasm_archive.rs`. Same fix.
7. **Mach-O: a C object's `__compact_unwind` is dropped** (`macho.rs` `classify`, 278-283).
   The debug check (`flags & S_ATTR_DEBUG != 0`, so `Dropped`) runs before the match on
   `("__LD", "__compact_unwind")`, and LLVM gives `__compact_unwind` exactly `S_ATTR_DEBUG`
   (`0x0200_0000`), so every clang object's unwind entries are dropped. `macho/tests.rs:526`
   asserts that `classify("__LD", "__compact_unwind", 0x0200_0000)` is `Unwind`, and item 11
   assumes it too. A fuzz target can't show this one (dropping is still a successful read).
   Fix: match `__compact_unwind` (and `__eh_frame`) before the debug check.

Not bugs, but the targets' allocation check may report them:

- A valid export trie that is one chain of terminal nodes has names totalling about len²/128
  bytes, and `trie_exports` pushes them all into one `Names` arena. The output needs those
  bytes, so either the reader bounds a trie's total name bytes or `link_macho_dylib`'s
  allocation bound has to change: say which.
- `trie_exports` pushes every edge of a node (up to 255) before any child is checked against
  `seen`, so overlapping nodes may grow `stack` past the bound before the cycle is found.
- The ELF archive reader keeps one `String` per index entry, so a BSD index of shared names
  costs quadratic memory in total, though no single allocation is large. The rayon threads of
  its unindexed path aren't counted by `largest_allocation` at all.

> sent to `link-next1` (no `link-pewasm-pe` or `link-macho-input` window was running); bugs 2
> and 6 also to `link-pewasm-wasm-next1`.

### 13. Two differential entries, for speed-link-edge-macho-dylib's request (safety.md, 2026-10-29)

speed-link-edge-macho-dylib asked (in `v2/requests/safety.md`) for `.tbd` and export-trie
targets checked against the readers kept in their tests: `tbd::tests::reference::read`, for
platforms 1, 2 and 6, and `dylib::tests::reference_trie_exports`. Both references are
`#[cfg(test)]`, and `fuzz_read` fixes platform 1, so a target can't reach either. Asked: the
two references compiled outside `cfg(test)` (say `macho/tbd/reference.rs` and the trie walk
beside `trie_exports`, used by the tests and these entries), and in `lib.rs`:

```rust
/// `macho::tbd::read("t.tbd", text, arch, platform)` against the reference reader: `Ok` with
/// whether it accepted when both give the same symbols, inputs and install name or the same
/// error (or the reference refuses nesting past 256), and the arena is exact
/// (`arena_is_exact`); `Err` naming both answers otherwise.
#[doc(hidden)]
pub fn fuzz_tbd_differential(text: &str, arch: blaze_codegen::Arch, platform: u32) -> Result<bool, String>;

/// `trie_exports(trie, 0, sections)` against the reference walk, `sections` being
/// `dylib::tests::test_sections`: `Ok` with whether it accepted when the exports or the errors
/// are equal, `Err` naming both answers otherwise.
#[doc(hidden)]
pub fn fuzz_trie_differential(trie: &[u8]) -> Result<bool, String>;
```

Until then `link_macho_tbd` and the new `link_macho_trie` (the input is the trie, placed in a
valid dylib) check no panics, the allocation bound, alignment, and that a trie reads alike
through `LC_DYLD_INFO_ONLY` and `LC_DYLD_EXPORTS_TRIE` for both arches. Their seeds are the
tests' generators with the tests' seeds: `generate` and `edges_match_reference`'s texts, and
`sample_trie` and `random_node`. `libSystem.B.tbd` is not a seed: it is Apple's and not in the
tree, and `seeds()` reads no files (`sdk_libsystem_tbd` covers it). When the entries land, I
switch both targets' reads to them, each platform in {1, 2, 6} for `.tbd`.

> sent to `link-next1` and `speed-link-edge-macho-next1` (no `speed-link-edge-macho-dylib`
> window was running).

## 13. From link-core-write: a write of shared bytes, and of several ranges at once

Where: `crates/blaze-core/src/lib.rs` (`Effect`), the io_uring, kqueue and threads backends.

Why: a link's file is already in memory when it is written. After a full link each region is
one buffer (`CowBytes` over a `Vec`), and `Effect::WriteAt` takes a `Vec<u8>`, so
`file::whole` copies every byte of the image once more into the runs it writes (8 MiB each,
aligned to 8 MiB, so they bypass the page cache under `DIRECT_LEAST`). An incremental link
writes many small ranges (a piece, an 8-byte jump-table cell), each a `Vec` and an effect.

Add:

```rust
    /// `parts` written in order from `at`, as one write (`pwritev` / `IORING_OP_WRITEV`):
    /// each a range of shared bytes, not copied. Completes with [`Done::Written`].
    WriteShared { file: FileName, at: u64, parts: Vec<(Mapping, std::ops::Range<usize>)>, tag: Tag },
```

With it a full link writes each region straight from its buffer and the headers from theirs
(no copy of the image), and an incremental link sends one effect per run of nearby ranges, each
range borrowed from the region's pages. Until then `file.rs` copies each run into its `Vec`
once, the fewest copies `WriteAt` allows.

> forwarded to link.
