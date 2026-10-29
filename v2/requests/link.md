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
`docs/v2/link-notes.md` ("Relocations codegen emits").

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
    /// C objects, archives and shared libraries, in link-line order (`Native::order`).
    pub natives: Vec<Native<'a>>,
    pub removed_natives: Vec<ContentId>,
```

`Image::new` becomes `pub fn new(to: FileName, output: Output) -> Image`.

`crates/blaze-link/Cargo.toml` gains `blaze-items.workspace = true` (for `ItemKey`) and
`blaze-format.workspace = true` (for `SpanRef`), both in the workspace already.

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
