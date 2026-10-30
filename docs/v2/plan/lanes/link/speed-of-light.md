---
status: plan
summary: lane link's speed of light: the bound each component's work and the hardware set, the arithmetic, and how the design reaches 90% of it.
date: 2026-10-29
---
# Lane link: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md). Each component has a section here, written by the
agent that owns it. Each edits only its own section.

## The machine

The target machines are in [hardware.md](../../design/hardware.md): EPYC Turin, Graviton 5 and
Apple M-series. Unless a section says otherwise, the figures are these, not yet measured:

| what | figure |
|---|---|
| clock | 3.5 GHz |
| L1 read, streaming | 64 B a cycle |
| L2 read, streaming | 32 B a cycle (about 110 GB/s) |
| one core from DRAM, streaming | about 25 GB/s |
| a socket from DRAM | about 400 GB/s (Turin, 12 channels of DDR5 at 70% of peak) |
| cores a link may use | 16 (a dev machine), 64 (a CI machine) |
| page-cache copy (`read`, `write`, `copy_file_range`) | about 10 GB/s a core |
| NVMe write | about 7 GB/s |
| system call on an open descriptor | 0.15 µs |
| system call that resolves a path | 0.4 µs |
| `mmap` plus first touch of a 4 KiB page | 0.25 µs a page |
| `mprotect` | 0.5 µs, plus a TLB shootdown of about 2 µs on each other core running the process |
| IPC | about 2 instructions a cycle for branchy code, about 4 for straight-line integer code |
| SHA-256 with SHA-NI or ARMv8 crypto | about 2 GB/s a core |
| BLAKE3 with AVX-512 | about 6 GB/s a core |

The reference workloads:
- **A full link of a large Rust binary** (ripgrep-sized to rustc-sized): 50 to 500 MB of input
  sections, 1 to 10 million relocations, 0.2 to 2 million symbols, and 100 MB of output.
- **An edit**: one function's body changes, so 1 to 10 functions are re-placed.

## ELF writer and readers: `crates/blaze-link/src/elf.rs`, `elf/` (speed-link-edge-sol-link)

**Workload.** A Linux x86-64 PIE of 100 MB: 200,000 named functions (80-byte names), 300,000
slid addresses, 210,000 FDEs in an 8 MB `.eh_frame`, 400 imports, and 300 MB of DWARF in a debug
build. Native inputs: the crt objects, `libc.so.6` (3,000 dynamic symbols), `libgcc_s`, and 20 MB
of C archives of which 500 members are pulled (60,000 symbols, 250,000 relocations, 3 MB of
string tables). The edit workload is one function's body changing.

The readers only hand out ranges and `Names` spans into the input; the writer is a pure function
of `Placed`. So every bound below is the memory the phase must touch, or its instruction count
where that is larger, spread over 16 cores. Budgets are the bound × 1.1. The figures are
arithmetic, not yet measured: this pass ran no benchmarks.

| phase | bound | where it is | what the core spends |
|---|---|---|---|
| parse | 0.1 ms a pull round | near the bound | about 2 ms copying inputs |
| resolve | 0.15 ms | near the bound | none |
| layout of synthetic sizes | O(1): sizes come from `Counts` | at the bound | none |
| synthesize `.rela.dyn` | 0.1 ms | near the bound | none |
| synthesize `.eh_frame_hdr` | 0.05 ms | near the bound (was 12 ms) | none |
| synthesize `.symtab`/`.strtab` | 0.1 ms | serial, about 1 ms | `Class::Named` strings |
| synthesize `.dynsym`/`.dynstr`/`.gnu.hash` | under 10 µs | at the bound | none |
| debug sections | 0 | at the bound: no copy (was 30 ms) | 19.2 made |
| relink one function | about 3 µs | O(changes) patched, plus one copy of each kept table (about 3 ms) | `Writer::synthesize` and `File::parts` own their bytes |

**Parse.** About 24 MB is touched: the headers, symbol tables, string tables and relocations of
the pulled members, plus the archive indexes. At 400 GB/s a socket that is 0.06 ms; the decoding
is about 16 million instructions, or 16M ÷ (3 × 3.5 GHz × 16) = 0.1 ms, so parse is compute-bound
at 0.1 ms each pull round. The string-table NUL scan, the last scalar per-byte loop, is now
eight bytes a step: `elf/read/strings.rs` `nul_mask` builds each 64-bit NUL bitmap with the SWAR
zero-byte test and a multiply gather. `elf/` forbids unsafe code, so this is the widest safe form.
It is tested against the old byte loop on every byte value at every position and 20,000 random
chunks. The core spends more than the reader: `native.rs` copies every native input into
`Input::bytes: Arc<[u8]>` (22 MB at 10 GB/s, about 2 ms of a core) and formats a member name for
each member read. Requests 19.3 (`Native::bytes: Arc<[u8]>` over the mapping) and 19.4 (name a
member only on an error) remove both.

**Resolve.** 60,000 symbols with about two hash probes each at about 20 ns a miss is 2.4 ms of
one core, 0.15 ms on 16. The symbol tables are already parallel and the names are spans, so
the cost is near the bound; a shared name index across inputs (link.md item 7, open) removes the
second probe.

**Layout.** Every synthetic size is a function of `Counts`, which the readers fill while parsing,
so layout does constant work for each region.

**Synthesize.**
- `.rela.dyn`: 300,000 × 24 bytes = 7.2 MB written and 2.4 MB of addresses read, plus a radix
  sort when the addresses come unsorted (8 passes over 2.4 MB, read and write): about 45 MB ÷
  400 GB/s = 0.1 ms. The writer is parallel and radix-sorted already.
- `.eh_frame_hdr`: 210,000 entries of 8 bytes out, 8 MB of `.eh_frame` in. Reading is 8 MB ÷
  400 GB/s = 0.02 ms; decoding is about 40 instructions an FDE, 8.4M ÷ (3 × 3.5 GHz × 16) =
  0.05 ms. It was a serial gimli walk with a CIE hash map, about 12 ms. Now `elf/dynamic.rs`
  splits the section at every 4,096th FDE start the core placed (`Function::fde`), walks the
  pieces in parallel with a hand decoder (CIE versions 1, 3 and 4, augmentations `z`, `L`, `P`,
  `R` and `S`, a zero length ending the section, as gimli reads them), joins the pieces in
  parallel, skips the sort when the table is already sorted (it is when functions are placed in
  address order), and encodes the table in parallel. A split that doesn't land on a record
  start is an error naming the offset, never a quiet serial walk. It is tested against the old
  gimli walk on random sections of five CIE kinds with splits every 1, 7, 64 and n FDEs, with
  terminators in any piece, and with the first error reported in section order.
- `.symtab`/`.strtab`: 200,000 × 24 bytes = 4.8 MB of symbols and 16 MB of names written, 16 MB
  of names read: about 37 MB ÷ 400 GB/s = 0.1 ms. It is serial today, about 1 ms, because the
  names are `Class::Named` `String`s the core owns; interning them (link.md item 11, being done by
  the core) lets the writer fill both tables in parallel from prefix sums of the name lengths.
- `.dynsym`/`.dynstr`/`.gnu.hash`: 400 imports and a few exports, under 10 µs.
- Debug sections: 300 MB. The bound for the linker is zero: the bytes are already in the inputs
  and in the store. They were one serial copy into the tail buffer (300 MB at 10 GB/s = 30 ms),
  then a parallel copy into a part each (about 2 ms). Now (19.2) `File::debug` names where each
  of the core's sections goes, (offset, index into `Placed::debug`), and `elf/image.rs` copies
  none: `file.rs` lays and diffs them from the state's shared `lists.debug`, and an unchanged
  section is the same buffer, compared by address. `elf/tests.rs`
  `the_debug_sections_are_the_cores_not_copies` checks the file's bytes against the core's and
  that 1 MiB and 3 MiB of debug info allocate the same buffers, under 4 KiB apart.

**Incremental relink of one changed function.** What changes: the body (about 1 KB), its FDE
(about 40 bytes), its `.eh_frame_hdr` entry (8 bytes), its `.symtab` entry (24 bytes) and a few
relocations and slid addresses, about 2 KB in at most five 4 KiB blocks. The bound is writing
those: five block writes at 0.15 µs a call plus 0.4 µs of copying, about 3 µs. `file.rs` diffs
parts at the same offset by 4 KiB block as well as synthetic regions (19.1), so a moved
`.symtab` entry rewrites its block of the tail part, not the 20 MB part. With 19.5 `Placed`
carries a `Delta` (the functions, slid addresses, GOT cells and binds spliced, the address
ranges the core wrote, whether regions and names are unchanged, and the synthesis it is from),
and an incremental output's writer keeps its last `.rela.dyn`, `.eh_frame_hdr` and tail
(`Elf::last`, each table with the synthesis it was made in) and patches them when the delta is
from that synthesis, no region moved and the lists it reads were spliced at equal lengths.
Every patched table is the one a build of the same `Placed` makes, which the tests check
byte for byte against a fresh writer's build; where that can't be shown cheaply the table is
built.
- `.rela.dyn`: the `RELATIVE` run is kept in the order of `Placed::rebase` (only when the
  addresses came sorted, so entry `i` is `rebase[i]`'s). A patch rewrites the entries of each
  rebase splice and of each slid address whose 8 bytes a `written` range or a spliced local GOT
  cell covers, two binary searches a range, and checks each splice is in order with its
  neighbours, so a build would sort nothing. The binds and GOT cells (a few hundred) are built
  as before and `.dynamic` with them.
- `.eh_frame_hdr`: the table is kept when it came in address order, so it is sorted by FDE as
  well. For each `written` range in `.eh_frame` the records are walked again with the build's
  own `entry`/`walk`, from the last kept FDE before the range to the first record start past
  it where a kept FDE starts (or the section's end or its zero length): the walk a build makes
  from there. Its FDEs replace that span's, and the patch holds only when no CIE was written,
  the FDEs start where they did (so the build's parallel splits stay record starts), the table
  stays sorted, and every FDE a spliced function names is one of the table (checked once on
  the build, in parallel, for every function, where the build splits at all).
- `.symtab`/`.strtab`: per function the writer keeps its local's index, name offset and stored
  name length. A functions splice whose functions keep their name presence and name lengths
  rewrites only those locals and names in the kept tail; the exports, imports, `.shstrtab`
  and section headers are written whole (O(globals + sections)); any change of layout (tail
  offset, debug length, counts, name totals) builds the tail.

So a one-function relink does O(splices + written + globals) work in the tables. What it
still spends is one copy of each kept table into the bytes it returns (7.2 MB of `.rela.dyn`,
1.7 MB of `.eh_frame_hdr` and about 21 MB of tail, 3 ms at 10 GB/s on one core, overlapped by
the core's block diff), because `Writer::synthesize` returns owned `Vec<u8>`s and
`File::parts` owns its bytes. Returning shared bytes (the core's `CowBytes`) would remove it;
that is an interface change in `model.rs` for the core's owner.

**Proof.** Each phase is timed on the workload above, a full link and a one-function edit, against
the bound × 1.1 in the table; the relink is also measured in bytes written, against the 2 KB.

**State of the art.** mold parses and synthesizes in parallel and copies debug sections with
parallel `memcpy` into a mapped output; it has no incremental relink. wild is parallel by design,
and incremental linking is its stated aim but is not done. lld builds `.eh_frame_hdr` serially
from its own `.eh_frame` parse. The MSVC incremental linker patches in place by padding functions
and calling through thunks, which changes the code; blaze patches without thunks, so the image is
the one a full link produces. With 19.1, 19.2 and 19.5, blaze's relink patches its tables in
O(change) (neither mold nor wild relinks incrementally); the copy of each kept table into the
returned bytes is what keeps it off the bound.

## Mach-O writer and readers: `crates/blaze-link/src/macho.rs`, `macho/` (speed-link-edge-sol-link)

**Workload.** The ELF workload built for arm64 macOS: a 100 MB image, 200,000 functions with
80-byte names, 300,000 rebases, 400 binds, 210,000 FDEs, and 300 MB of DWARF in `__DWARF`. The
native inputs are the same C archives plus `libSystem.tbd` (about 1 MB of YAML in about 40
documents, 30,000 names) and any dylibs. The files were held by the `requests-link-fuzz` agents
during this pass, so the changes below are filed as exact edits in
`v2/requests/requests-link-fuzz.md` (A1 to A5) and `v2/requests/link.md` (19.1 to 19.5) rather
than made. Budgets are the bound × 1.1; the figures are arithmetic, not yet measured.

| phase | bound | where it is | filed as |
|---|---|---|---|
| parse objects and archives | 0.1 ms a pull round | near the bound | 19.3, 19.4 |
| parse `.tbd` | 0.02 ms | serial, about 0.3 ms | A6 |
| resolve | 0.15 ms | near the bound | link.md item 7 |
| layout of synthetic sizes | O(1) | at the bound | none |
| export trie | under 10 µs for an executable | at the bound | none |
| rebases and binds | 0.02 ms | serial opcodes, 0.3 ms; chained (A2 made) O(slots) | none |
| `__unwind_info` | 0.06 ms | serial, about 1 ms; a relink patches it (19.5 made) | A8 |
| symbol table | 0.1 ms | locals parallel; `String` names | link.md item 11 |
| code signature | 3.2 ms (16 cores) | about 1.05 × the bound | none (A3 made) |
| `__DWARF` | 0 | at the bound: no copy | none (A5 made) |
| relink one function | about 15 µs | O(changes) hashed and patched, plus one copy of `__LINKEDIT` | none (19.5, A4 made) |

**Parse.** Objects and archives cost as in the ELF section, and the core's input copy is the same
(19.3, 19.4). A `.tbd` is about 1 MB scanned at about a byte a cycle: 0.3 ms of one core. Its
documents are independent, so parsing them in parallel is 0.3 ms ÷ 16 ≈ 0.02 ms (A6).

**Resolve** is the ELF figure, 0.15 ms.

**Synthesize.**
- Export trie: an executable exports a handful of symbols, under 10 µs. A dylib with 30,000
  exports sorts them by a radix key of the first eight name bytes, then builds the trie in one
  pass: about 0.05 ms.
- Rebases and binds: 300,000 rebases are 2.4 MB of addresses in and, as opcodes, under 1 MB out:
  about 0.02 ms in parallel. Today one serial opcode stream, about 0.3 ms. With chained fixups
  (`LC_DYLD_CHAINED_FIXUPS`, pointer format `DYLD_CHAINED_PTR_64_OFFSET`) there is no rebase table:
  each slot carries its own fixup and the next slot's distance, written with the slot by
  `Writer::encode`, and the only table is a 2-byte page start for each of 25,600 pages, 51 KB.
  The core lays and diffs encoded slots (`file.rs` `changed_slots`), and `macho/chained.rs`
  (A2) is the Mach-O `encode` and the `LC_DYLD_CHAINED_FIXUPS` table, with
  `LC_DYLD_EXPORTS_TRIE` in place of `LC_DYLD_INFO_ONLY`. It is chosen as ld-prime chooses:
  macOS 12, iOS, tvOS and Mac Catalyst 15, watchOS 8, DriverKit 21 and every visionOS; other
  platforms and earlier targets keep the opcodes. rustc's defaults are 11.0 (arm64) and 10.12
  (x86-64), and blaze's is 11.0, so the default bytes are unchanged and both paths must be at
  the bound. Imports use `DYLD_CHAINED_IMPORT`, or `_ADDEND` when an addend doesn't fit the
  pointer's signed byte; a slot not 8-aligned, outside `__DATA_CONST`/`__DATA` or both slid
  and bound is an error. `macho/tests.rs` `chained_fixups_slide_and_bind_what_the_opcodes_do`
  decodes the chains by hand (the workspace's `object`, 0.37, reads none) against the opcodes
  of the same image at 11.0, and `a_bind_added_rewrites_two_slots` checks that a new bind
  changes its slot and its predecessor's only.
- `__unwind_info`: 200,000 functions, 800 KB of second-level entries: about 0.06 ms. It is a serial
  `.eh_frame` walk and a serial build today, about 1 ms. A8 moves the split parallel scan now in
  `elf/dynamic.rs` into the core's `unwind.rs` as one function both writers use, and builds the
  second-level pages in parallel. On a relink (19.5) `unwind::build_kept` keeps the last
  table's columns, rows and pages (`unwind::Last`, moved out of the full build, no copy but
  the table's bytes and the functions' addresses) and patches it for a `Delta` from its
  synthesis: it reads again only the `__eh_frame` records and `__compact_unwind` entries
  `written` overlaps, moves each changed FDE or entry in its column in place, and merges again
  only from each changed address to the first unchanged entry after it. It rewrites the rows,
  LSDA entries, pages (at most 4 KiB each) and first-level entries that changed, and the
  sentinel. It builds whole when a row would come or go, a row gains or loses its LSDA, a page
  would split differently, the common encodings or a personality change, a changed FDE or entry
  is not alone at its address or leaves its place in its column, a CIE's encoding or a record's
  length changes, a bind of a personality cell changes, or the last end shrinks. That is
  O(splices + entries written + pages touched × 1,021 rows) plus one 800 KB copy (about 40 µs),
  because `Writer::synthesize` returns owned bytes. `unwind.rs`'s tests hold each patch to
  `build`'s bytes: a moved function, a moved LSDA and the last FDE grown; a delta from another
  synthesis; an unequal splice; a lost row and a new LSDA; and random LSDA moves over the
  seeded images.
- Symbol table: 200,000 `nlist_64` of 16 bytes (3.2 MB) and 16 MB of names: about 0.1 ms
  memory-bound. The locals were one serial loop, 3 to 5 ms. Now `macho.rs` `locals_into` fills
  them in parallel, one task per 4,096 locals at offsets from prefix sums of the name lengths,
  and the first error is the first local's in order (A1). It is tested against the old loop on
  0 to 10,000 random names, both spellings, and an error in several chunks. The `exported`
  probe per function costs no hash when the set is empty, as in every executable, because
  hashbrown returns early from an empty table. What is left is the core's `Class::Named`
  `String`s, one pointer chase a name, which interning (link.md item 11) removes.
- Code signature: 100 MB ÷ 4 KiB = 25,600 pages. SHA-256 of a 4,096-byte page is 64 blocks plus
  one padding block, so the bound is 100 MB ÷ (2 GB/s × 16) × 65/64 = 3.1 ms × 1.016 = 3.2 ms
  (0.8 ms on 64 cores), budget 3.5 ms. `macho/sign.rs` hashes 64 pages a task in parallel, about
  1.05 × the bound. `macho.rs` copied the signature into one part per 4 KiB page so a relink
  rewrote only changed pages; now that `file.rs` diffs a part by block (19.1), the signature is
  one part with the same relink writes and no allocation per page (A3). The pages hash the
  chained slots laid over the regions (`sign::View::with_slots`).
- `__DWARF`: the bound is zero for the reason given in the ELF section. It was one 300 MB serial
  copy, 30 ms; now (A5) `image` lays the core's sections through `File::debug` at the same
  offsets and the signature hashes them where they are. `macho/tests.rs`
  `the_debug_sections_are_the_cores_not_copies` checks their bytes and that only the
  signature's hashes grow with them.

**Incremental relink of one changed function.** The ELF changes, plus the rebase or chained slot
words on the changed pages and the signature: about 2 KB in a few pages, and two to three pages
to rehash at 4 KiB ÷ 2 GB/s = 2 µs each, plus one code-directory slot each. The bound is about
15 µs. The writer regenerated every synthetic region (5 to 40 ms, most of it the `__DWARF` copy
and the serial symbol table), and the signature cache compared all 25,600 pages (about 0.5 ms).
Now, for an incremental output, `macho.rs` keeps its last tables (`Kept`) and uses them only for
a `Delta::from` equal to the synthesis they are of (a rollback brings back a state they are not
of), with the same regions:
- The rebase and bind opcodes, the trie and the function starts are the kept bytes when their
  lists have no splice (and, for binds and the trie, the names are the same).
- The symbol table's locals are patched in place (`patch_locals`) for equal-length function
  splices with names of the same length, local or not alike and still in order; otherwise it
  is built again. `__unwind_info` is patched by `unwind::build_kept`.
- The signature (A4) rehashes the pages that overlap the delta's `written` (which includes the
  synthetic regions' changed blocks), the pages of chained slots that changed, and the debug
  sections that aren't the same buffers. It compares only the headers' and `__LINKEDIT`'s
  pages, which are all it keeps, and keeps every other hash.

`the_signing_cache_writes_the_same_file` checks five relinks, at macOS 11 and 12, against an
uncached writer, and `a_relink_patches_the_symbol_table_as_a_full_build_writes_it` that a patch
keeps the kept buffer and gives a full build's bytes. What a relink still spends is copying
`__LINKEDIT` into the part it returns and into `Kept`'s page store (tens of MB at 10 GB/s,
about 2 ms), and diffing the kept chained slots (16 bytes a slot).

**Proof.** As for ELF: each phase timed on the workload, a full link and a one-function edit,
against the bound × 1.1, with the signature also timed alone on 25,600 pages against 3.5 ms on 16
cores, and the relink measured in pages hashed and bytes written.

**State of the art.** ld-prime (Apple's ld since Xcode 15) is parallel and emits chained fixups,
and signs in parallel; it has no incremental relink. lld's Mach-O port builds the export trie,
`__unwind_info` and the signature (`CodeSignatureSection`, parallel over pages) much as blaze does,
with serial symbol tables. mold's Mach-O port (sold) is parallel throughout. None relinks one
function in O(change); after 19.1, 19.5, A2 and A4 blaze does, and A5, A6, A8 and link.md item 11 bring each
full-link phase within 10% of its bound.

**Made, and filed.** 19.1, 19.2, 19.5, A2, A4 and A5 are made (requests-link-next-writers, not
yet built: the lane made them without a build, so the tests above are written and not yet run).
A6 and A8 are in `v2/requests/requests-link-fuzz.md`.

## The live runtime: `crates/blaze-live/` (speed-link-edge-sol-live)

The runtime that takes patches is `blaze_live::slots::Live` (the jump table: a stub per
function loads the table cell, then its slot, then jumps). `blaze_live::Live` (v1, a jump
written over each replaced function's entry) is still used by blaze-engine, the demos and the
`live*` harnesses; it is measured here only for its stop. Figures are from "The machine",
except where a line says otherwise, and none is measured yet. `T` is the number of other cores
running the program; `8` where a total needs a number.

Workloads:
- **One-function edit**: about 200 B of code, 1 cell, 1 FDE (about 40 B), a literal or two,
  and a message of about 1 KiB.
- **1,000-function patch**: about 200 KB of code, 1,000 cells, 1,000 FDEs (about 40 KB),
  about 50 KB of data, and a message of about 300 KB. The program's table has `S` slots, one
  per function of the image; `S` = 100,000 is 800 KB.

### A patch's apply, from message to new code running

The bound, on Linux x86-64:
- **Read the message once**: `openat` (0.4 µs) + `statx` (0.15) + one `read` (0.15 +
  bytes ÷ 10 GB/s) + `close` (0.15) = 0.85 µs + 0.1 ns a byte. That is 0.95 µs for the edit
  and 31 µs for the 1,000-function patch. One read into a buffer sized by `statx` is the least
  that reads a file of unknown length once: without the size it takes a second `read` that
  returns 0, which costs the same. Checking and walking the bytes in memory is about
  300 KB ÷ 110 GB/s = 3 µs.
- **Code**: copy the bytes once, through a writable second mapping of the patch region: 0
  system calls, 0 shootdowns, 200 KB at about 50 GB/s = 4 µs. x86 needs no cache maintenance
  for code at addresses no core has run.
- **Data the patch writes in place** (new statics, relocated words, the image's data cells):
  in writable pages, 0 system calls.
- **Cells**: `C` relaxed 8-byte stores into a table the cell doesn't point at, then **one
  release store** of the cell, which is the switch. That is 1,000 stores, 1 to 8 µs depending
  on how much of the table is in cache.
- **Frames**: one `__register_frame` per patch's run of FDEs (libgcc reads the run when it
  first unwinds through it).
- **The answer, written once**: `openat` with `O_CREAT` (0.4) + `write` (0.15) + `close`
  (0.15) + `rename` (about 0.6, two paths). That is 1.3 µs, and it is the least that publishes
  a file whole.

Bound for the edit: 0.95 + 1.3 + about 0.1 = **2.4 µs**, budget **2.6 µs**. Bound for the
1,000-function patch: 31 + 3 + 4 + 8 + 1.3 = **47 µs**, budget **52 µs**. On AArch64 the
bound adds the cache maintenance the core asks for (`CTR_EL0`): nothing but `dsb ishst` and
`isb` when IDC and DIC are both set, or else one `dc cvau` and one `ic ivau` per 64-byte line
written, about 3,200 lines of each for 200 KB.

Where it was:
- The message took 5 system calls: a probe `read` after the sized one.
- Each patch stored into the standby table all the cells of every generation since that table
  last held the current one, about `4C` stores with a pool of 4 tables, all before the switch.
- The first three patches each copied a whole table (`S` × 8 B, 800 KB, about 80 µs) before
  their switch.
- On macOS every data span cost two `dladdr` calls, which scan the image's symbol table
  linearly: about 100 µs each with 100,000 symbols, so roughly 200 ms for a 1,000-span patch.
  On Windows every span cost a `VirtualQuery`.
- On AArch64 the per-line `dc` and `ic` loops ran even when `CTR_EL0` said they weren't needed.
- Read-only pages a patch writes (new FDEs in `.eh_frame`, literals in `.rodata`, RELRO and
  GOT words): one `mprotect` pair per run of pages, with a shootdown when write is taken away:
  1 µs + 2 µs × `T`, about 17 µs a run at `T` = 8. An edit touches about two runs, so about
  34 µs, 14 times the bound. On macOS each run of code or read-only data is a page swap
  (`mach_vm_allocate`, a copy of whole 16 KiB pages, `mach_vm_protect`, `mach_vm_remap`,
  `mach_vm_deallocate`, `sys_icache_invalidate`). On Windows each run is a `VirtualProtect`
  pair and a `FlushInstructionCache`.

What changed (speed-link-edge-sol-live, 2026-10-29):
- `slots.rs` reads the message with one sized `read` (4 system calls) into a buffer and a file
  name kept across patches, through `patch::message_file_into`.
- `switch.rs`: after each switch, `Switch::ready` brings a table to the current generation, or
  makes one while the pool isn't full (one at start). The next patch then stores only its own
  `C` cells before its switch, and the catch-up and the table copies run after it. Tested by
  `a_patch_stores_only_its_own_cells_before_the_switch`. After a rollback, the next patch still
  catches its table up before the switch.
- `unix.rs` `Segments`: the image's loaded segments are found once (`dl_iterate_phdr` on Linux,
  the Mach-O load commands on macOS), and each span is checked by binary search.
  `windows.rs` reuses one `VirtualQuery` per region.
- `linux.rs` `flush` honors `CTR_EL0` IDC and DIC (Linux hides DIC on Neoverse N1, erratum
  1542419).

Where it is: the edit on Linux x86-64 is the bound plus the read-only runs, about 2.4 + 34 µs.
[Request 19 of `v2/requests/link.md`](../../../../../v2/requests/link.md) has the linker place
a patch's new FDEs, literals and RELRO pieces in the patch's own regions, which takes the runs
to 0 and the edit to the bound. macOS keeps a page swap per merged run: about 5 traps and a
16 KiB copy, around 10 µs. Its bound is a second, writable mapping of the patch region made at
start, as on Linux: 0 traps and only `sys_icache_invalidate` over the bytes written. That is
not built, because it can't be checked without running it. Windows has the same bound, with a
section mapped twice.

State of the art: Live++ and MSVC `/hotpatch` write a jump over the old entry and suspend every
thread to check its instruction pointer (v1's design). JITs on Apple Silicon (V8,
JavaScriptCore) switch W^X per thread with `pthread_jit_write_protect_np`, which needs
`MAP_JIT`, and `MAP_JIT` can't be mapped at a fixed address. Linux's own JITs write through a
second mapping, as here. Dioxus `subsecond` swaps a jump table too, but it reads the patch as a
whole shared library through `dlopen`.

### A call through a slot

The x86-64 stub is `mov r11, [rip+cell]; jmp [r11+disp32]`: two dependent loads and an
indirect jump. The bound for a table that switches all its slots in one store is one load more
than a PLT stub (`jmp [rip+slot]`): the cell, one line shared by every stub and so always in L1.
When the branch predictor knows the target, the loads only check its guess, so the bound is
**0 extra cycles** over a direct call. When it guesses wrong, the loads add 2 × 4 to 5 cycles
of L1 latency to the 15 to 20 cycles of the misprediction. Budget: no more than 0.5 cycles a
call over a direct call, measured in a predicted loop (0 × 1.1 is below what a loop can
resolve). x86-64 is at the bound. AArch64 takes 6 instructions (`adrp`, `ldr`, `movz`, `movk`,
`ldr`, `br`); request 10.3 of `link.md` asks for 4 (`adrp`, `ldr` of the cell, `ldr` of the
slot at a scaled offset, `br`), and that is the AArch64 bound, still 2 loads.

### v1's stop-the-world: `apply::write_stopped`

Work on Linux x86-64, per patch:
- **Per other thread**: a real-time signal by `tgkill`, the signal delivery, a compare-and-swap
  on the shared count, a futex wait and wake, `cpuid` to serialize (a VM exit under a
  hypervisor, about 1 to 2 µs) and `sigreturn`. That is about 3 to 10 µs of the stopping
  thread's time per thread.
- **Per patch**: `/proc/self/task` listed at least twice and `/proc/self/maps` read once (10 to
  50 µs for a large program), and an `mprotect` pair with a shootdown per run.
- **Retries**: up to 20 rounds, 500 µs apart, while any thread is inside the bytes a jump
  covers.

The least that makes a write over arbitrary prologues safe still interrupts every thread to
read its pc: one signal round trip per thread, sent to all at once, about 0.2 µs × `T` plus the
slowest handler. The state of the art, Linux's `text_poke_bp` (an `int3`, then three broadcasts
of a core-serializing IPI), costs tens of µs whatever the thread count, but only works where
the patch site is one instruction. The slots design needs neither: **0 per thread**, the cell's
store. So the fix is moving v1's users onto slots, not a faster stop. `slots::Live` now has
what they need (the macOS sampler, the directory `tiers()` reads, and `apply_as_they_come`), and
[request 20 of `link.md`](../../../../../v2/requests/link.md) asks for the daemon's side and the
users' switch, after which v1 is removed. Request 15's blind spots go with it.

### Rollback, and the answer written once

Going back from generation `g` to a kept `g'`: when a table of the pool holds `g'` (any of the
last three, with 4 tables), **0 slot stores and 1 release store** of the cell. Otherwise one
relaxed store per cell that differs between the two, undoing down to their common ancestor and
redoing up, then the cell. Then the answer: the same 4 system calls, one file, written once and
renamed into place. That is the bound and where the code is. Budget: 1.1 × (cells differing ×
store + 1.3 µs).

### The log of generations: reserved columns (speed-link-edge-mem-live)

The switch keeps every generation the program ran (`switch.rs` `Log`) and every lasting range
(`Lasting`) in columns. They were `Vec`s with room for 1,024 generations, 16,384 cells, 4,096
writes of code and 4,096 lasting ranges. Past that, a column doubled: one allocation, plus a
copy of the whole column, on the latency path of the patch that crossed the line. The
allocation-budget test only held below those counts. blaze-mem's `RegionVec` can't be used
(`v2/requests/mem-backend-live.md`, "refused by requests"): blaze-live is published and built
alone into user programs, and blaze-mem reserved about 264 GB, which broke `Live::open` under
`ulimit -v` and valgrind.

Now each column is a `slots/column.rs` `Column<T>`:
- At `Live::open` it reserves address space for its most rows (`mmap` with `PROT_NONE` and
  `MAP_NORESERVE`, or `VirtualAlloc` with `MEM_RESERVE`).
- It commits 256 KiB at a time as it grows (`mprotect` to read and write, or `MEM_COMMIT`), so
  rows never move and growing never calls the allocator.
- A full column refuses the patch before anything is written, with an error naming the column
  and its limit. Nothing falls back to the heap.

The `Range` columns became pairs of `u64` columns, and `kept` became one byte per generation.
Rows are 25 B per generation, 20 B per cell, 16 B per write of code and 25 B per lasting range.
The limits are `switch.rs` `Limits::LONG`; each column is rounded up to the 256 KiB step:

| column | rows | bytes per row | reserved |
|---|---|---|---|
| generation numbers | 2^20 | 8 | 8 MiB |
| parents, depths | 2^20 each | 4 | 4 MiB each |
| kept marks | 2^20 | 1 | 1 MiB |
| cell runs, code runs | 2^20 + 1 each | 4 | 4.25 MiB each |
| slots | 2^24 | 4 | 64 MiB |
| new targets, old targets | 2^24 each | 8 | 128 MiB each |
| code starts, code ends | 2^22 each | 8 | 32 MiB each |
| lasting starts, ends, generations | 2^20 each | 8 | 8 MiB each |
| lasting kinds | 2^20 | 1 | 1 MiB |

That is 25.5 MiB for the per-generation columns, 320 MiB for cells, 64 MiB for code and
25 MiB for lasting ranges: **434.5 MiB of address space in 15 mappings**. None of it is
memory until written. `Live::open` therefore needs `ulimit -v` (which counts reservations, in
KiB) to leave 444,928 KiB above the program's own mappings: a 1 GiB limit fits a program
whose own mappings are under about 590 MiB, and 4 GiB or unlimited always fits. Windows charges
only committed pages against the commit limit, and macOS doesn't enforce `RLIMIT_AS`.

How long the limits last:
- A one-function edit takes 1 generation, 1 cell, 1 write of code and up to 3 lasting ranges
  (the code, its FDE and the zero length after the FDE's run).
- So the lasting ranges fill first, after about 350,000 edits: an edit every 10 seconds for
  40 days.
- A 1,000-function patch takes about 1,000 cells, 1,000 writes and 3,000 lasting ranges, so
  about 350 of them fit.
- A full log refuses the patch, naming the column, and the program needs a restart. No program
  has come near this.

Commits are rare:
- A one-function edit writes 136 B across the 15 columns (25 B per generation, 20 for its cell,
  16 for its code and 75 for 3 lasting ranges). Each column commits once per 256 KiB it
  fills, so that is **one `mprotect` per 1,928 edits**.
- A 1,000-function patch writes about 111 KB: 0.42 `mprotect` per patch. Each call adds
  access to untouched pages, so no TLB shootdown, about 1 µs.

The first write to a committed page faults (a minor fault, about 0.25 µs with zeroing). That
is once per 30 one-function edits, but about 27 times for a 1,000-function patch: about 7 µs,
15% of its 47 µs bound. So after each switch, off the latency path, `Column::ahead` commits
and writes once each page that a next patch of the same size would push to. The next patch's
pushes then find their pages committed and resident, and its log work costs only the stores:
111 KB at about 50 GB/s is 2 µs, inside the bound's 10%. The commits move behind the switch
too.

Proof:
- `a_warm_patch_allocates_nothing` takes 9,000 warm patches of 4 cells under a budget of 0
  allocations. That is past the old 1,024 generations and 16,384 cells, and across a commit
  step of every cell column.
- `a_full_column_refuses_the_patch` sets small limits and checks that a full generation,
  cell, code or lasting column refuses the patch whole and names itself.
- `column.rs`'s model test checks `Column` against a `Vec` over seeded pushes, splices and
  truncations, across commit steps and up to the limit.
- `a_column_costs_memory_only_where_written` runs per platform: `mincore` on Linux and macOS,
  `VirtualQuery` and `QueryWorkingSetEx` on Windows. It checks that a reserved or committed
  column has no resident page until a row is written.

State of the art: wasmtime and V8 reserve each linear memory or heap up front, with guard
regions in GiB, and commit as it grows. This is the same technique sized for a program that
links it: hundreds of MiB, not GiB.

### The sampler's counters: `sample.rs`

The sampler reads no counter per sample. When it writes the profile (every 250 ms) it makes
`used` relaxed loads of `blaze_live_counters`, at most 65,536 × 8 B = 512 KiB. At 25 GB/s that
is 21 µs per 250 ms, 0.008% of a core, and it is the bound: each counter read once.

The program's side costs more. Level-2 code does a plain load, add and store per counter, and
the functions' counters are packed eight to a line, so two hot functions on two cores move one
line back and forth (about 50 to 100 ns each time, against about one cycle). The bound is about
one cycle an add: each function's counters on their own 128 bytes, and per-thread blocks when
one function is hot on many threads. That is codegen's (`hir-ty-codegen/tier.rs` `count`):
filed in `v2/requests/codegen.md`.
