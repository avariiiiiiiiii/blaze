---
status: plan
summary: How blaze-link works as written by lane link (the core, the jump table, positions, live patching, native resolution, PE and wasm rules) and what it still lacks; nothing in it was built or run.
date: 2026-10-29
---
# link: notes on the in-place linker

Part of [link](../link.md). Code: `crates/blaze-link/` (the core, and the format writers
behind `model::Writer`: `elf*`, `macho*`, `pe*`, `wasm*`), `crates/blaze-live/` (the runtime
that takes patches) and `vendor/cranelift-object/`. Nothing here was built, checked or run (v2
rule: agents run nothing expensive); every claim is read off the code.

## A link

`Image::new(to, output)` makes an empty image; `Image::stage(Change)` applies a change to a
copy-on-write copy of the state (`CowVec`, `Arc` fields) and returns `Staged`, the effects to
run; `Image::commit(staged, effects)` adopts it once the caller ran them, `complete` takes the
caller's completions, `roll_back` goes back one commit. A stage is, in this order
(`stage.rs`):

1. removed functions, statics and natives go; added ones become pieces, in parallel, their
   symbols interned shard by shard (`Symbols`, 16 shards);
2. `native.rs` resolves every name from scratch, as lld would for the same inputs (below);
3. `needs` makes the GOT cells and PLT stubs the pieces to write need;
4. `sweep` counts references to shared pieces (data, CIEs, file names) and drops unused ones;
5. `place.rs` places (a full link lays everything out; an incremental one allocates);
6. `write.rs` relocates every piece that is new or refers to something that moved, in
   parallel, each core on a disjoint slice of its region's bytes;
7. `file.rs` has the writer synthesize its regions and headers, and makes the effects: a full
   link truncates and writes the whole file; an incremental one clones the old file (FICLONE)
   and writes only the dirty ranges, then `Sync`, `Mode 0o755` (a clone may not keep the
   mode), `Rename` over the output. The undo of a commit is the old bytes of exactly the
   ranges it wrote, clipped to the old length, and the old length.

A full link is the same whatever the core count or the order changes came in: each class's
pieces are sorted by rank, link-line order, then symbol key.

## Relocations

What codegen emits and what the linker makes of it (`reloc.rs`, `reloc::of_kind`). `S` is the
target, `A` the addend, `P` the place, `G` the target's GOT cell.

### Relocations codegen emits

| `RelocKind` | x86-64 | AArch64 | written as |
|---|---|---|---|
| `Abs64` | yes | yes | `S + A`, 8 bytes; rebased by the loader when `S` is in the image |
| `PcRel32` | yes | no | `X86Branch32`: `S + A - P`; a call to an import goes to its PLT stub |
| `GotPcRel32` | yes | no | `G + A - P`; the cell is made by `needs` |
| `Call26` | no | yes | `A64Call26`; out of range is an error naming the call |
| `AdrpPage21`, `PageOffset12` | no | yes | page and low 12 bits; the low-12 shift is read from the instruction |
| `GotPage21`, `GotPageOffset12` | no | yes | the same, of `G` |
| `TlsOffset` | yes | yes | `S + A` from the thread pointer, by the format's TLS variant |
| `WasmFunction`, `WasmTable`, `WasmMemory` | wasm32 only | | `wasm::codegen_apply(kind, in_code)`: in code `FUNCTION_INDEX_LEB`, `TABLE_INDEX_SLEB`, `MEMORY_ADDR_SLEB`; in data and statics the `_I32` forms |

Targets: `Target::Function(key)` is the function's jump-table slot when the image has a table
(incremental, not wasm), else its body; `Target::Direct(key)` is always the body;
`Target::Extern(name)` a name natives and Rust share; `Target::Weak(name)` the same, 0 when
nothing defines it; `Target::Static`, `Target::Data` codegen's pieces; `Target::Position(span)`
a `core::panic::Location` record ("Positions"). A value that does not fit its field is an
error naming the field, never a truncation.

## The jump table

An incremental image calls every function through a slot (`table.rs`, #275):

- `SymKey::Generation`: one 8-byte cell holding the address of the current cells.
- `SymKey::Cells`: one 8-byte entry per slot, the address of the function now in it.
- `SymKey::Stubs`: one stub per slot, what callers call. x86-64 (16 bytes):
  `mov r11, [rip+generation]; jmp [r11 + slot*8]`. AArch64 (24 bytes): `adrp x16; ldr x16`
  (the generation cell), `movz/movk x17` (the slot offset), `ldr x16, [x16, x17]; br x16`.
  Only call-clobbered registers no argument is in (`r11`; `x16`, `x17`).
- Capacity: `max(2n, n + 65536)` slots for the first link's `n` functions; a full table is an
  error asking for a link from scratch.

An incremental link writes changed entries, never moves a caller. A live patch writes a new
cells array and one store of its address into the generation cell switches every slot at
once; a rollback stores the old address.

`Change::slots` is `(slot, function)` pairs: the slot of the first runs the second. An edited
function has a new `InstanceKey` (its body changed) while its callers name the old one, so
codegen and the coordinator pass, for an edit, the old key in `removed`, the new function in
`added`, and `(old key, new key)` in `slots` (as codegen's `TieredUp::replaced` gives for a
tier-up). Removing a function empties its slot only where the slot still holds that body; a
removed function whose slot is left empty while something still calls it is an error naming
the caller. Slots need a table: `slots` on an image linked without one is an error.

## Positions

`line!()`, `file!()`, `column!()`, a panic's and a `#[track_caller]` function's `Location` are a
`Target::Position(SpanRef)` relocation, never bytes in code (`position.rs`). The linker makes a
24-byte record per span, as `core::panic::Location` is laid out: the file name's pointer at 0
and length at 8, `LINE` at 16, `COLUMN` at 20 (`position::LINE`, `COLUMN`), from the span table
`Change::spans` carries for the span's anchor (magic `BZSPANS1`). File names are NUL-terminated
shared pieces. A reformat changes only the table: each record of the anchor is rewritten in
place and no function is compiled or relocated again. DWARF line tables (`Debug::Lines`) come
from `CompiledFunction::lines` through the same tables (`dwarf.rs`).

## Live patching

`blaze-live` (`patch.rs`) is the protocol: a `Descriptor` (version 2, 112 bytes, with the
generation) exported as `__blaze_live`, and `Message::{Patch, RollBack, Restart}` files in the
live directory, `message_file(n)`, written under another name and renamed. `live.rs` makes a
`Patch` of an incremental link: base and new generation, the code and data writes, the new
cells, frames and Windows unwind entries; anything a running program can't take (a moved
static, a changed layout, new imports) is a `Restart` naming why. ELF frames are followed by a
4-byte zero inside the patch's writes (libgcc and libunwind read a run of FDEs to a terminator).
New code never reuses old code space: a running program may still be in it, and a rollback
returns to it. A panic under the runtime's guard rolls the newest generation with a frame in
its code back to its base and answers `RolledBack`; `Image::roll_back` follows.

## Start and stop sections

`__start_X` and `__stop_X` (linkme, `#1496`) are `Special::Section { name: X, end }`: the bounds
of the `Class::Named` regions of that name; a reference to one when no input has the section is
an error naming the section. Named and init-array regions are ordered: an incremental link that
adds or removes a piece in one lays the region out again in place.

## Native inputs

`native.rs` loads objects in link-line order and pulls archive members for what is strongly
undefined, wherever the archive is on the line, to a fixed point (lld's rule). Then:

- COMDAT: of a name the first group wins; a group marked `largest` (`IMAGE_COMDAT_SELECT_LARGEST`)
  is replaced by a later one whose first section is larger, and candidates in a dropped group
  are forgotten.
- A strong definition beats a weak one beats a common; two strong ones are an error naming both
  inputs; a Rust definition and a strong native one are an error.
- `SymbolDef::Fallback` (COFF weak externals, `/ALTERNATENAME`): when nothing defines the name
  it is an alias of its fallback symbol, which must then be defined; `search` makes the name
  pull archive members; a fallback chain back to itself is an error.
- `.drectve` directives: `/INCLUDE` roots and requires its symbol; `/EXPORT:name=symbol` exports
  it (from an executable too, as link.exe does); `/FAILIFMISMATCH` with two values is an error
  naming both inputs; `/DEFAULTLIB` and `/NODEFAULTLIB` are `default_libraries`, which the
  caller resolves on its library path.
- An archive member that reads as a `Shared` (an import library's member) adds its DLL, merged
  by name with the others of that DLL.
- Collection (`--gc-sections`): from the roots (Rust's references, the entry, exports, `_init`,
  `_fini`, retained and init/fini sections), through relocations; a kept group's sections live
  or go together; unwind sections and FDEs are kept for what they describe.
- What is left undefined: a writer's special, a shared library's export (an import), 0 when
  every reference is weak (executables), an undefined import of a shared image, else an error
  naming the symbol and who refers to it.

## PE

`Plan::got_by_library`: the GOT is the import address table. Import cells are sorted by DLL and
import, each DLL's run followed by an 8-byte zero piece (`SymKey::IatEnd`), local cells after;
the GOT is laid out again in place when it changes. `__imp_X` is `Special::Got(X)`: X's cell,
X resolved strongly. Every reference to an imported function other than through its cell goes
to its PLT stub (`jmp [rip+cell]`, link.exe's thunk); imported data other than through `__imp_`
is an error. The core writes nothing in import cells: the loader fills them. A DLL's entry is
`Writer::shared_entry` (`_DllMainCRTStartup`). `Writer::own` puts codegen's thread-locals in
`.tls$`. `Placed::symbol` lets the writer find `_tls_used`, `_load_config_used`.

## wasm

Code is reached by index (`Plan::indexed_code`): every class the writer makes executable, and
its executable synthetic regions (`__wasm_call_ctors`), are a code space of their own from 0,
apart from memory. A function's index is the number of imports (all wasm imports are
functions) plus its rank by code address; `FUNCTION_INDEX_*` get the index, `TABLE_INDEX_*` the
index + 1 (its slot), 0 for a weak undefined function; a call by index to an undefined one is an
error. An undefined name that some input says the wasm import of (`Symbol::import` of a native
object, `Code::imports` of a codegen function, `Static::imports`) and that is not weak becomes a
function import (rustc's `--allow-undefined`); any other undefined name is an error.
`Placed::functions` lists native code pieces too. No jump table, no GOT, no DWARF, no live
patching (a live wasm output is an error).

## Tests

`crates/blaze-link/src/tests.rs` links x86-64 ELF images end to end through a simulated disk
(it runs the effects on a sparse file) and reads them back: calls reach their callees; the image
is the same in any order; an incremental link clones, writes little, and moves only a slot
entry; a rollback restores the file byte for byte; a failed commit leaves the image as it was;
live descriptors and patches; positions and a reformat that rewrites only them;
`__start_`/`__stop_`; C and Rust calling each other by name; DWARF line tables. Each format's
writer has its own tests from hand-made `Placed` values. None was run.

## Limits

- The whole cells array (8 bytes per slot of capacity, about 512 KiB at the least capacity) is
  written on each incremental link of an image with a table; writing only changed entries is a
  smaller write the effects can already say.
- `default_libraries` reads objects on the link line only: libraries a pulled archive member
  asks for are not found (link.exe follows them).
- An archive member is pulled even when a shared library given earlier defines the name.
- AArch64: no range-extension veneers; a call past ±128 MiB is an error naming it
  (`Class::Veneers` exists in the model but the core makes none).
- PE: no PDB (debuggers and dbghelp show addresses only), no delay-load imports, no
  `/guard:cf` tables (see the PE writer's report).
- wasm: no DWARF; a direct call to an undefined weak function is an error, where wasm-ld makes
  a trapping stub.
- Zero-copy reading of native inputs (`Data` borrowing the input's bytes) is requested by the
  speed lanes and not done: every reader still copies section bytes once.
