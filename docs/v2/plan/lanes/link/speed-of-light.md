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
| debug sections | 0 | 3 ms parallel copy (was 30 ms) | request 19.2 |
| relink one function | about 3 µs | 3 to 20 ms | request 19.5 (19.1 landed) |

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
  and in the store. They were one serial copy into the tail buffer (300 MB at 10 GB/s = 30 ms);
  now `elf/image.rs` makes each debug section its own part, copied in parallel 1 MiB chunks
  (300 MB ÷ 160 GB/s ≈ 2 ms), so the file layer skips an unchanged debug section on a relink.
  Request 19.2 (`File::debug` naming input ranges the file layer writes or clones itself)
  removes the copy.

**Incremental relink of one changed function.** What changes: the body (about 1 KB), its FDE
(about 40 bytes), its `.eh_frame_hdr` entry (8 bytes), its `.symtab` entry (24 bytes) and a few
relocations and slid addresses, about 2 KB in at most five 4 KiB blocks. The bound is writing
those: five block writes at 0.15 µs a call plus 0.4 µs of copying, about 3 µs. Today the writer
regenerates every synthetic region from `Placed` (O(image), 3 to 20 ms). `file.rs` now diffs
parts at the same offset by 4 KiB block as well as synthetic regions (19.1, landed), so a moved
`.symtab` entry rewrites its block of the tail part, not the 20 MB part. What is left is 19.5,
which gives `Placed` a `Delta` (the functions
spliced, the rebase, GOT and bind changes, and which names and regions are unchanged) so the
writer overwrites in place. `.symtab` order is free, so a moved function's entry is rewritten
where it is; a removed `.eh_frame_hdr` entry becomes a copy of its predecessor and the new entry
goes at the end before a re-sort of only the changed tail, so the table keeps its length and the
binary search stays valid.

**Proof.** Each phase is timed on the workload above, a full link and a one-function edit, against
the bound × 1.1 in the table; the relink is also measured in bytes written, against the 2 KB.

**State of the art.** mold parses and synthesizes in parallel and copies debug sections with
parallel `memcpy` into a mapped output; it has no incremental relink. wild is parallel by design,
and incremental linking is its stated aim but is not done. lld builds `.eh_frame_hdr` serially
from its own `.eh_frame` parse. The MSVC incremental linker patches in place by padding functions
and calling through thunks, which changes the code; blaze patches without thunks, so the image is
the one a full link produces. After 19.1, 19.2 and 19.5, blaze is at the bound everywhere above,
which neither mold nor wild reaches on a relink.

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
| rebases and binds | 0.02 ms | serial opcodes, 0.3 ms | A2 (chained fixups) |
| `__unwind_info` | 0.06 ms | serial, about 1 ms | A8 |
| symbol table | 0.1 ms | locals parallel; `String` names | link.md item 11 |
| code signature | 3.2 ms (16 cores) | about 1.05 × the bound | none (A3 made) |
| `__DWARF` | 0 | 30 ms serial copy | A5, 19.2 |
| relink one function | about 15 µs | 5 to 40 ms | 19.1, 19.5, A4 |

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
  The core already lays and diffs encoded slots (`file.rs` `changed_slots`); A2 specifies the
  Mach-O `encode`. Chained fixups need a deployment target of macOS 12 or later, and rustc's
  defaults are 11.0 (arm64) and 10.12 (x86-64), so opcodes stay the default for those, as ld64
  chooses, and both paths must be at the bound.
- `__unwind_info`: 200,000 functions, 800 KB of second-level entries: about 0.06 ms. It is a serial
  `.eh_frame` walk and a serial build today, about 1 ms. A8 moves the split parallel scan now in
  `elf/dynamic.rs` into the core's `unwind.rs` as one function both writers use, and builds the
  second-level pages in parallel.
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
  one part with the same relink writes and no allocation per page (A3).
- `__DWARF`: the bound is zero for the reason given in the ELF section; today one 300 MB serial
  copy, 30 ms. A5 drops the copy once 19.2 lands.

**Incremental relink of one changed function.** The ELF changes, plus the rebase or chained slot
words on the changed pages and the signature: about 2 KB in a few pages, and two to three pages
to rehash at 4 KiB ÷ 2 GB/s = 2 µs each, plus one code-directory slot each. The bound is about
15 µs. Today the writer regenerates every synthetic region (5 to 40 ms, most of it the
`__DWARF` copy and the serial symbol table), and the signature cache compares all 25,600 pages
(about 0.5 ms). With 19.5's `Delta`, A4 rehashes only the pages written, and chained fixups make
a moved function's fixups the slot words it already rewrites.

**Proof.** As for ELF: each phase timed on the workload, a full link and a one-function edit,
against the bound × 1.1, with the signature also timed alone on 25,600 pages against 3.5 ms on 16
cores, and the relink measured in pages hashed and bytes written.

**State of the art.** ld-prime (Apple's ld since Xcode 15) is parallel and emits chained fixups,
and signs in parallel; it has no incremental relink. lld's Mach-O port builds the export trie,
`__unwind_info` and the signature (`CodeSignatureSection`, parallel over pages) much as blaze does,
with serial symbol tables. mold's Mach-O port (sold) is parallel throughout. None relinks one
function in O(change); after 19.1, 19.5, A2 and A4 blaze does, and A5, A6, A8 and link.md item 11 bring each
full-link phase within 10% of its bound.

**Filed, not made.** A2 (chained fixups) is a new output form with its own reader test, left to
the holder of `macho.rs`'s fuzz work to make with a build. A4 and A5 wait on link.md items 19.5
and 19.2, whose types core-model-next1 is adding; 19.1 has landed. A6 and A8 are in `v2/requests/requests-link-fuzz.md`.

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
