---
status: plan
summary: The Mach-O object and archive readers (`macho/input.rs`, `macho/archive.rs`) against mold, wild, ld-prime and lld; what they cost before and after, and what the contract still forces.
date: 2026-10-29
---

# Mach-O object and archive reading, at the speed of the fastest linkers

Paths: `crates/blaze-link/src/macho/input.rs`, `crates/blaze-link/src/macho/archive.rs`. Both
run on every cold link, once per C object and once per archive (and once per archive member a
link pulls), so a large crate graph runs them thousands of times (libc-like crates,
compiler-builtins, ring, openssl).

`n` is the object's nlist entries, `r` its relocations, `a` its atoms, `S` its Mach-O sections,
`b` the symbols that bound an atom in a section, `F` the FDEs of `__eh_frame`, `C` its CIEs,
`m` an archive's members, `k` its index entries.

## The benchmark

- **mold/sold** (`macho/input-files.cc`): the nlist, string and relocation tables are borrowed
  from the mapped file; subsections are split by sorting the symbols by address once; a
  relocation's atom is a binary search of the sorted subsection starts; one `Vec` of
  relocations per subsection, sized once.
- **wild**: section bytes and names are never copied; every per-object table is a column
  indexed by `u32`, preallocated from counts in the headers.
- **lld's Mach-O `ObjFile`**: `parseSymbols` sorts each section's symbol indexes by address
  once and splits subsections from that; `findContainingSubsection` is a binary search.
- **Apple's ld-prime**: objects parsed in parallel, one pass each, tables read in place.

So the target: tables borrowed, one pass over the symbol table into preallocated columns, atoms
from one sort, every address-to-atom lookup a binary search, relocations decoded in place with a
tag `match`, `__eh_frame` scanned once, allocations a constant per object plus what the contract
forces per atom and per symbol.

## Objects (`input.rs`)

| | before | after |
|---|---|---|
| nlist table | a `Vec<&Nlist>` of the table (8 B a symbol) | borrowed `&[Nlist]` from the file |
| name column | grown `Vec<&[u8]>` | `reserve_exact(n)`, one allocation |
| atom starts and bounds | two `Vec<u64>` per Mach-O section (`2S` allocations), each sorted | one `(section, offset)` sort for the file, then two columns of the whole file (`bounds`, `starts`), each section a `Range<u32>`/`first..first+atoms` of them: 3 allocations, 8 B a bound and 8 B an atom |
| a symbol's size | linear scan of its section's bounds: `O(b)` a symbol, `O(b²)` a section | binary search: `O(log b)` |
| atom of an offset | binary search of the section's starts | the same, over its range of the one column |
| nlist → symbol map | `Vec<Option<u32>>`, 8 B a symbol | `Vec<u32>` with a `u32::MAX` sentinel, 4 B |
| section symbols | `Vec<Option<u32>>`, 8 B an atom | `Vec<u32>`, 4 B |
| `symbols` | grown from empty | `with_capacity(n)` |
| relocation table | copied into a `Vec<RelocationInfo>` per section (one allocation and 12 B a relocation) | read in place: one branch-light scan for `R_SCATTERED`, each entry decoded where a reader takes it; the CPU picked once per section, not per relocation |
| decoding into `Apply` | `u8` tag `match` on `(r_type, r_pcrel, r_length)` | unchanged: already a tag match, no `dyn`, no `String` but on the error path |
| fixups | pushed into each atom's `Vec` (amortized growth: up to `log` reallocations an atom), then each atom's `Vec` sorted | pushed into one column sized from `r` up front, one stable sort by `(atom, offset)`, each atom's run collected with its exact length: one allocation an atom with fixups, none reallocated |
| `__eh_frame` covered fields | `Vec` of the atom's fixup offsets, `contains` per field: `O(F·r)` | sorted, binary search: `O(F log r)` |
| `__eh_frame` FDE → CIE | linear scan of the CIEs: `O(F·C)` | binary search (CIEs are seen in offset order): `O(F log C)` |
| `__eh_frame` pointer → section | linear scan of the sections per field: `O(F·S)` | sections sorted by address once, when the first uncovered pc-relative pointer needs it, then binary search: `O(F log S)` |
| `__eh_frame` error text | a `format!` of the section's name on every call, error or not | formatted only on an error |
| `LC_LINKER_OPTION` | a `Vec<&str>` then `join` | one `String`, sized from the command |

Passes over the nlist table: two (atom bounds, then symbols), as lld and mold make (the bounds
must be known before any symbol's atom and size). Each is a straight walk over the borrowed
table and the name column.

Error-path behaviour kept the same, with one deliberate addition: an object whose non-empty
sections overlap in address is refused when its `__eh_frame` has a pc-relative pointer without a
relocation ("sections X and Y overlap"), since which atom an address names would be ambiguous.
Before, the first section in file order was taken. clang and `ld -r` never write such objects.

### What the contract still forces (and the change that removes it)

Per atom: a `String` for `Section::name` (a clone of the Mach-O section's `SEG,SECT`), a
`Vec<u8>` copy of its bytes (`Data::Bytes`), a `Vec<NativeFixup>` when it has fixups. Per
symbol: a `String` (`Symbol::name`, from `core_name`). mold and wild allocate none of these.

- `v2/requests/link.md` item 5 (open, lead `link`): `Data::Input(Range<u64>)` takes the bytes
  copy away (the ranges are valid against the slice `Writer::read` was given even when this
  reader copies it to an 8-aligned buffer, as offsets are the same), and the `names` arena with
  `Name { at, len }` takes the per-symbol `String` away. Once applied, the reader edits are: the
  atom's `Data::Bytes(bytes[start..end].to_vec())` becomes `Data::Input(base + start..base + end)`
  with `base` the section's file offset (`header.offset(LE)`); `core_name` writes into
  `Object::names` and returns a `Name`.
- Not yet requested by anyone, filed with my parent: `Section::name` as a `Name` into the same
  arena (one `SEG,SECT` per Mach-O section, shared by its atoms), and `Section::fixups` as a
  range of one per-object `Vec<NativeFixup>` (CSR), which removes the per-atom `Vec`.

After those three, an object read allocates a constant number of times (the columns above, the
arena, the object's `sections`/`symbols`/`fixups`), as wild does.

## Archives (`archive.rs`)

| | before | after |
|---|---|---|
| `__.SYMDEF` / GNU `/` index | read in place through `object` | unchanged |
| members | listed in place; one `String` a member name (the contract's `Member::name`) | unchanged |
| index entry → member | `ArchiveFile::member(offset)` re-parsed the member's header (and its `#1/` name) for every index entry, then a binary search of the members' data offsets | binary search of the members' header offsets, taken in place from `ArchiveMember::header()` while listing: no parse per entry, `O(log m)` |
| index `Vec` | grown from empty | `with_capacity` from the iterator's exact `size_hint` |
| index names | one `String` each (`core_name`, the contract's `(String, u32)`) | unchanged |

The core turns the index into an `FxHashMap<&str, u32>` once per archive (`native.rs`), so
lookup by name is one hash. What remains per entry is the `String`; with item 5's arena an
archive's index becomes one `names` string and `(Name, u32)` pairs, as mold keeps them.

## unsafe

Neither file has `unsafe`; each forbids it at module level (`#![forbid(unsafe_code)]`), as the
crate can't while `macho.rs` has one block. Every zero-copy view of the input is made after a
bounds check: `object`'s checked parsers for the headers and tables, `get(..)` for relocation
fields, `Cursor::take` for `__eh_frame`, a length check for archive members
([unsafe.md](../../design/unsafe.md)).

## Tests

- `input.rs`: every existing test kept. Added `seeded_objects_read_as_the_per_section_model_reads_them`:
  256 seeded objects (text, data, cstring and zerofill sections; global, local, `ltmp`, `L`, `l`
  and `N_ALT_ENTRY` symbols at random places, at section ends too; `BRANCH26`, extern and
  non-extern `UNSIGNED` relocations in a shuffled table) read by the reader and by a model of the
  old layout (a `Vec` of starts and bounds per section, linear scans, per-atom sort): the same
  atoms, alignments, bytes, fixups and symbols. And `eh_frame_fdes_find_their_cie_and_section_by_search`:
  two CIEs, three FDEs pointing into atoms of two sections, each `pc_begin` made explicit to the
  right atom.
- `archive.rs`: added 64 seeded BSD archives (1 to 20 members, short and long names, up to 50
  index entries) whose index must map to the members written, and an index entry naming no
  member header refused.
