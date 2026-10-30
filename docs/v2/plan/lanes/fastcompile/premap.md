---
status: plan
summary: blaze v2 lane fastcompile, idea 7 (dependencies as one premapped image): crates/blaze-premap, the key over everything an image's bytes depend on, the split of a dev build into image and workspace pieces, the prelink at a fixed base through a blaze-link hook, the image format and its symbol table by instance key, the one memory object per key mapped by every run, the run's start that makes image plus workspace behave as one whole link (statics, TLS, `#[used]`, init order, weak symbols, dlopen), rebuilds only when the key changes, the hooks requested of link, codegen, inproc and cargo, and the state of the art it matches or beats.
date: 2026-10-29
---
# fastcompile: dependencies as one premapped image (`crates/blaze-premap`)

Idea 7 of [compile-time.md](../../design/compile-time.md). Everything below the workspace
members is prelinked once into one image, keyed by the dependency graph and mapped by every
dev run, so a dev link places only the workspace's own code. Owned here: `crates/blaze-premap/`
and this file. Hooks in other lanes are requests in `v2/requests/fastcompile.md`
(`fastcompile-premap 1` to `6`).

## The state of the art, and where this stands

| system | what it shares | how this compares |
| --- | --- | --- |
| Darwin's dyld shared cache | every system library, prelinked at one address, mapped by every process | the same shape for a build's dependencies: one image per key, mapped at a fixed base, data copy-on-write, binds done once per run. Unlike the cache, an image is rebuilt in seconds when its key changes, and it is never slid, so no rebase pass runs at all. |
| prelink(8), Solaris `crle` | libraries' relocations resolved ahead at fixed addresses | the same, without prelink's failure modes: no address conflicts (one base, chosen far from every loader's), no undo when a library changes (the key changes and a new image is made) and no stale result (the key covers every input). |
| mold, wild, lld `--threads` | a fast whole link | blaze-link already links in parallel; the image removes the dependency part of every dev link (most of the bytes: thousands of dependency functions against tens of workspace ones), so a dev link's cost is the workspace's code plus one table probe per outside reference. No linker with a full link per change can match a link of only the changed side. |
| Bazel / Buck "prebuilt libraries", sccache | object files of dependencies, cached | they still link everything each time; here the dependency link is done once per key, machine-wide, and kept in the store. |
| cargo's dylib dependencies (`-C prefer-dynamic`, bevy's `dynamic_linking`) | dependencies in one shared library | the nearest Rust practice. It changes the program (another `std` copy rule, `dylib` crate types, symbol interposition, a dynamic loader in the path) and isn't a whole link's behavior. Here the run behaves as the whole link does in every case below, and nothing about the program changes. |

## What an image is keyed by

`key.rs`. Two keys, BLAKE3 with their own domains, every input hashed by destructuring its type
(a field added is a compile error in `key.rs` until it is hashed), every list sorted and
length-prefixed (rustflags excepted: their order is their meaning).

- `GraphKey` (`"blaze v2 premap graph 2"`): for every dependency unit, its package id, target,
  features, cfgs and rustflags. Then:
  - the toolchain (`settings::Toolchain`: the nightly's and blaze's code identity, triple, arch,
    cpu, target features, backend versions, frame pointers, relocation model, stencils);
  - every codegen settings id the image's functions were compiled under;
  - blaze-link's code identity;
  - the output it links for (format, arch, kind and entry or name and exports, interpreter,
    platform and versions, incremental, debug info, live, tags, stack, Windows subsystem, the
    dev run's arena, cell and fixed sizes or none, triple, exec log);
  - the base address;
  - the run loader's thread-local bias;
  - every native input of the link line: content id, kind (object, archive and whether whole,
    shared library and whether as-needed) and place on the line;
  - the span side tables positions are filled from.
- `ImageKey` (`"blaze v2 premap image 2"`, the store's key and the memory object's name): the
  graph key, then every function by instance key, code hash and the digest of the code hashes
  it inlined (`inlined_digest`), and every static by item,
  instance and definition (value, relocations, alignment, mutability, thread-local, section,
  the data it points at). Then every global name the image defines and what it is, and every
  debug name.

Images of one graph differ only in which instances they hold: a build that needs an instance
the graph's image lacks links it with the workspace this time (`spilled`), and the image of its
own key is made in the background.

Tested: `graph_key_changes_with_every_input` changes each of the 40 inputs alone and checks
every key differs from all the others; `image_key_changes_with_every_instance` does the same
for the image key's 13; `graph_key_ignores_order_of_sets` checks order-free lists are and
rustflags aren't. Both keys build their sorted records on a `Tag::Link` frame:
`a_graph_key_allocates_nothing_of_the_heap` and `an_image_key_allocates_nothing_of_the_heap`.

## Which pieces are the image's

`split.rs`. An instance is the dependencies' when its item is a dependency's (or std's) and
none of its type arguments names a workspace type, closure or item. Codegen knows this when it
makes the instance; it says so per instance with the generic's code hash (`Built::owners`,
`fastcompile-premap 3`). So `Vec<my::Type>::push` is the workspace's. It is made only for this
workspace, and putting it in the image would make the image depend on workspace code. Statics
aren't generic and belong to their crate.

- `image_part` takes the dependency pieces and checks the image is closed. A dependency
  function or static that reaches a workspace instance or static is an error naming both;
  ownership as above rules it out. Names outside blaze's code (`Extern`, `Weak`) stay open,
  bound by the run (`__rg_alloc` from the workspace's `#[global_allocator]`, `malloc` from
  libc).
- `workspace_part`, given an image, takes everything its table lacks. Every piece is in
  exactly one of the two, so every function and static has one address. A piece the table
  holds must be the image's own, checked against the slot's content and debug references
  without computing the image key: a function's inlined digest, a static's definition, a debug
  name, and every Rust name the image defines (its value and kind, and none the build lacks).
  An init or fini entry of the image with Rust's tie must be one of the build's statics. Any
  other is `Fit::Stale`, and the build makes its own image. Pieces are sorted into the lists in
  parallel above 4096, each list allocated once at its length
  (`a_hit_allocates_its_lists_and_nothing_per_piece`); the parts borrow the build's names
  (`splitting_a_build_copies_no_name`).

## The prelink and the image

`prelink.rs`, `image.rs`, `table.rs`. blaze-link links the image as a shared output
(`OutputKind::Shared`, never incremental, never live) at the fixed base `0x40_0000_0000`, the
start of `prelink::IMAGES`. That window starts where blaze-inproc's `BASES` ends, so it never
collides with other code memory a run maps. It ends at `0x80_0000_0000`, the top of a 39-bit
address space (the smallest AArch64 Linux configures), and stays away from where loaders put
executables, libraries, stacks and heaps. The workspace image is made at a base just past the
dependency image, so calls between them stay within reach. It goes through a hook (`Prelink`,
`fastcompile-premap 1`) whose stage runs in parallel, as any full link does. `prelink::Link`
implements it with `blaze_link::premap::prelink`, whose types mirror premap's field for field.
The workspace image is linked by `blaze_link::Image::beside` against the dependency image's
`View`, which is its `blaze_link::Outside` (`fastcompile-premap 2`). What comes back is
checked against what was asked:

- every function and static is placed;
- segments are aligned to the target's mapping granule (16 KiB on Apple silicon, 64 KiB on
  other AArch64 and on Windows, 4 KiB otherwise), in order and apart;
- every bind cell is an aligned word of a writable segment;
- every init piece, thread-local template, unwind range and Mach-O thread-local descriptor lies
  inside the image.

It is then encoded as one value: a 288-byte header (magic, version 2, format, arch, key, base,
granule, the count of Rust names it defines, a directory of 13 sections) and fixed-layout
records, the segments' bytes last and page-aligned. Everything before them is the metadata, and
the bytes are written in parallel 1 MiB pieces. `View::parse` checks every offset, length, name
and address against the bytes and the key before anything reads them.

The symbol table is open-addressed, at most half full, in 64-byte slots: id, value, size,
line order, kind, strength, and references (from 1, 0 for none) into the content ids and debug
names the image was made of. The prelink's placement check is one probe per piece. A slot is found by the first 8 bytes of its id: the instance key,
or a domain-separated hash of a static's item and instance or of a global name. A workspace
link's outside reference costs one probe, usually one cache line. Parsing checks every slot is
reachable from its probe start and of a kind there is.

It is kept in the store as an output (`Kind::Output`, content-defined chunks), so two images of
one graph that differ by an instance share almost every chunk. The store checks a value's
content hash before handing it out. Tested: `store_round_trip` puts an image into a simulated
store, reads it back before and after it is durable and after a reopen, parses it, and checks
that other bytes under the same key are refused.

## Mapping

`map.rs`, on blaze-inproc's `CodeMemory`. One memory object per image key on the machine holds
the image's address space from its base: object offset is address minus base, gaps and zero
tails are pages never written. Every run maps it with `FixedMap`s at their addresses:

- code: read and execute, shared;
- read-only data: shared;
- data: copy-on-write, so every run starts with the image's initial values and none sees
  another's writes.

The object is written and its code published once. Mapping a run costs one map call per
segment, a few in all, and the pages already in the page cache.

## Why the run behaves as the whole link

`start.rs` plans what a run does before its entry. Each case below is argued against what a
whole link of the same pieces does:

- **Statics' addresses.** A static is in exactly one side. The image's are at fixed addresses
  (the base is never slid). The workspace link resolves a dependency static through the table,
  so every reference reaches the one copy, as in a whole link. Read-only data is merged by
  content within each side, not across them. Rust promises no address identity for constants
  and promoted values, and rustc's codegen units already duplicate them, so this is behavior
  rustc's output has too.
- **Initial values.** The data segment is mapped copy-on-write from the image, so every run's
  statics start at the bytes the prelink wrote. Pointers in them were written at the fixed
  base, so no run relocates them.
- **Weak symbols and interposition.** A name the image leaves undefined, or defines only
  weakly, is a bind: an 8-byte cell the start writes. The rules are lld's resolution:
  - a strong definition beats a weak one;
  - of two weak ones, the first on the link line wins;
  - two strong ones are the whole link's duplicate-symbol error, here too;
  - with no definition, the shared libraries are searched in line order, and a weak reference
    gets 0.

  So a workspace `#[no_mangle]` that overrides a dependency's weak default (or the allocator
  shim's `__rg_alloc`) takes effect, as in a whole link.
- **Archive members.** The image pulled the members its side needed. The workspace link
  resolves against the image's table first, every global of a native object in the image
  included. So it pulls only members the image did not, and no member is in both (a whole link
  pulls each once). Every archive not taken whole, and every shared library, is on both sides'
  lines.
- **Shared libraries.** Each side lists what its link uses; the start loads the union in line
  order, each once. An `as_needed` library neither side uses is in neither list, as the whole
  link would drop it.
- **Thread-locals.** blaze-inproc's loader keeps one area in every thread's static block (a
  bias and a size). The image's thread-locals are at offset 0 of it, so offsets computed in its
  link alone hold in every run. The workspace's follow at the image's size rounded up to the
  workspace's alignment (`workspace_tls_offset`). The workspace link places them there, and the
  start refuses any other offset or an area too small. Each part's initial bytes come from its
  own template. On Mach-O the image's `__thread_vars` descriptors are passed to the loader to
  fill.
- **`#[used]`, `#[link_section]`.** Kept statics are pieces like any other: a dependency's
  `#[used]` static is in the image, and its section's bytes are in the image's segment for that
  section. `.init_array`/`.fini_array` pieces become init entries, below. Other named sections
  that a run enumerates through `__start_`/`__stop_` symbols (linkme-style distributed slices)
  are one range per link, and a range can't span two images. So the prelink refuses an image
  whose dependencies define such a section the workspace also adds to; see the limits below.
- **Init and fini order.** A whole link orders `.init_array` pieces by priority, then by
  input's place on the line (input, member, section), then by Rust's tie (item and instance),
  and runs a piece's pointers in address order. The start merges both sides' pieces by exactly
  that key, and two pieces with one place are refused. Fini runs last to first. Tested:
  `start_runs_init_in_whole_link_order_and_places_thread_locals`.
- **dlopen of C libraries.** The start loads the libraries with every symbol global, before the
  entry. A C library a dependency dlopens later finds the image's and the workspace's globals
  through the run process's global scope, which the loader populates from the table. Two
  references are refused rather than guessed:
  - a fixed thread-pointer offset (TpOff) into a thread-local only a shared library defines:
    such a library has no fixed offset, and a whole executable link would have refused or
    relaxed it to a dynamic access;
  - a bind kind that doesn't match the definition (an address bound to a thread-local or the
    reverse).
- **Unwinding and backtraces.** Each side's call frame information is registered with the run's
  unwinder (`unwind` ranges). The image's debug names are in its own debug info, and a
  backtrace through both resolves as in one image.

Tested in the model (`model.rs`, `prelinked_and_workspace_link_run_as_the_whole_link`). A small
graph has:

- two dependency functions, one calling the other;
- a dependency static pointing at a function;
- dependency and workspace thread-locals;
- dependency and workspace init pieces;
- a dependency reference to `malloc` (a library), to `__rg_alloc` (defined by the workspace) and
  to a weak `absent` (nothing);
- workspace references into the dependencies.

It is linked whole, and as the image plus the workspace link against it, with the run's start
applied. Every function's and static's address or offset, every byte the program starts with,
the init functions' order and the thread-local layout are equal. The model shares placement
with the whole link, so what it checks is resolution: table lookups, binds, start writes, TLS
offsets, init merge. That is all premap changes. `start_binds_as_a_whole_link_resolves` checks
the precedence rules, `image_refuses_a_dependency_that_reaches_into_the_workspace` checks the
closure, and `prelink_refuses_what_it_was_not_asked` and `image_round_trips_and_refuses_damage`
check the format.

## Rebuilding only when the key changes

`cache.rs`, `dev.rs`. The machine's `Images` holds one opened image per key (parsed, checked,
its memory object filled, only its metadata kept in memory), shared by every view and run,
never one per view. `dev::plan`:

1. keys the graph and takes the newest image of it;
2. if that image's pieces are the build's own (above) and it holds all of them, links the
   workspace against it, with no image key computed;
3. if it holds them but some spilled, links the spilled instances with the workspace, keys the
   build's own image and claims it to be made in the background;
4. if it is stale, or there is no image of the graph, keys the build's own image and uses it if
   open, else makes it (from the store if it is there, else by the prelink). Another build
   meanwhile waits rather than making it twice.

A finished image replaces its graph's previous one, which closes when no run maps it. At most 8
graphs are kept (a machine's branches and toolchains), the least recently used closed first. A
failed image fails again for its key: no whole link takes its place. Tested:
`an_image_is_made_once_per_key_and_again_only_when_a_build_needs_more`,
`an_image_serves_only_a_build_whose_pieces_it_holds_are_its_own`,
`a_build_the_newest_image_does_not_serve_makes_its_own` and
`a_failed_image_fails_again_and_never_falls_back`.

## Limits, each an error naming it

- AArch64 `BL` reaches ±128 MiB. A workspace call into the image across more than that needs
  the workspace image placed within reach (it is made at a base right after the image) or a veneer from
  blaze-link. Without either, blaze-link's range check fails the link; nothing is silently
  redirected.
- A TpOff reference from the image to a library's thread-local is refused (above).
- Thread-locals of both images are reached at fixed offsets from the thread pointer (initial-
  and local-exec). A dynamic access (a `__tls_get_addr` pair, a TLS descriptor such as
  AArch64's `TlsDescCall`) is blaze-link's error, never a slower path.
- The workspace executable is not slid (no PIE ASLR): its references into the image are
  absolute addresses.
- A PC-relative reference inside the image to a name it defines weakly can't be rebound to a
  stronger workspace definition. Only GOT cells and pointers in writable data are binds. This
  is not an error yet: a strong workspace definition of such a name must refuse the link
  (`fastcompile-sol-premap 3`), or the run differs from a whole link.
- A native section only the workspace refers to is not in the image: link its archive whole
  into the image, or name it.
- PE images register no `.eh_frame` ranges, and their CRT init sections aren't checked to be
  init arrays; this is untested.
- A `__start_`/`__stop_` section spanning both sides is refused (above).
- wasm has no image: code is reached by index, not mapped (`prelink::base`).

## Requests

In `v2/requests/fastcompile.md`:

1. link: the prelink hook and the workspace link's layout;
2. link: the outside symbol source;
3. codegen: `Built::owners`;
4. inproc: the start and the placement;
5. cargo: the dev-run path;
6. coordinator: the workspace dependency.

## Speed of light

Nothing here was run: costs are counted from the code and the machine. The machine and unit
costs are those of
[speed-of-light-infer-relate.md](../db/speed-of-light-infer-relate.md): an Apple M5 Max, 6
performance cores at about 4.5 GHz, 16 MB L2 per cluster, DRAM about 100 ns, sustained IPC
about 3.5. Memory bandwidth is about 10 GB/s from one core and well over 100 GB/s from all of
them.

The workload is a mid-size dev program:

- \(F \approx 100\,000\) dependency function instances and 2 000 workspace ones;
- \(S \approx 5\,000\) dependency statics, about 100 bytes each (0.5 MB);
- \(N \approx 2\,000\) Rust names;
- one debug name per function, about 60 bytes each;
- an image of \(W \approx 85\) MB with \(R \approx 1.4\) million relocations, about 800 000 of
  them to distinct targets;
- a table of about 210 000 entries in 512 Ki slots (32 MB). It is not in cache on a dev run:
  each probe is a random line of DRAM, overlapped by the core's memory-level parallelism.

### Unit costs

| step | cost |
|---|---|
| a table probe in a cold 32 MB table, about 25 in flight per core | about 4 ns |
| compare a 32-byte id or content ref (L1) | about 1 ns |
| read and compare a debug name of about 60 bytes (its line of the strings section) | about 9 ns |
| BLAKE3 of one short record: hasher, one block, finalize | about 80 to 100 ns |
| BLAKE3 streamed over long input, one core | about 1.5 GB/s |
| one `mmap` of a resident object at a fixed address | about 2 µs |
| a minor fault on a fresh zero page (16 KiB) | about 0.5 µs |
| apply one relocation: read the target from a hot array, patch 4 or 8 bytes | about 5 ns |

### The hit: deciding a dev run's image

A hit is a dev run whose dependencies are unchanged, the common case. The work that can't be
skipped, with codegen's interface as it is (it hands over pieces, not their digests):

- the graph key: a few hundred records of units and settings, sorted and hashed, about 30 µs;
- per built function, one probe to tell the image's from the workspace's, plus a compare of the
  slot's content ref against the digest of what it inlined;
- per debug name the image holds, one probe and one string compare;
- per static, its symbol id (one BLAKE3) and its definition (one BLAKE3 over its value and
  relocations), then a probe and a compare.

So the bound on one core is

\[ T_{\text{hit}} \approx 30\ \mu s + (4 + 1)\,F + (4 + 9)\,F + 200\,S\ \text{ns} \approx
0.03 + 0.5 + 1.3 + 1.0 \approx 2.8\ \text{ms}, \]

and on \(c\) cores, every term but the graph key split evenly,

\[ T_{\text{hit}}(c) \approx 30\ \mu s + 2.8\ \text{ms} / c \approx 0.5\ \text{ms at } c = 6. \]

Nothing in it allocates per piece: the lists the link takes are allocated once each, at their
lengths.

**What the code did.** `dev::plan` split the build and computed the image key on every run. It
sorted and hashed about 18 MB of records (every function's key and code hash, every static's
definition, every debug name) and cloned every name into the parts. That is about 95 ms on one
core, a sequential hash, about 35 times the one-core bound and 190 times the six-core one.

**What it does now:**

- It keys the graph first, on a `Tag::Link` frame with no heap allocation, and takes the
  newest image of the graph.
- `workspace_part` checks every piece against that image's slots as above, without computing
  the image key. It sorts pieces in parallel (rayon, in pieces of 4096) into one class byte
  each, then builds each list once at its exact length.
- Only a spilled or stale build computes the image key, and that is the cold path's cost.

The counted cost is the bound's terms plus one sequential pass over the class bytes to build the
lists, about 0.1 ms for 100 000 functions. On one core that is within 5% of the bound. On six
cores it is about 0.6 ms against 0.5 ms, 20% over. That pass stays sequential because a
parallel filter-collect allocates per split, and the budget test holds the hit to one
allocation per list (`a_hit_allocates_its_lists_and_nothing_per_piece`).

**Closer than the bound.** Statics cost a third of the bound because their definitions are
hashed on every run. Debug names cost half of it because they are compared as strings. If
codegen gave a sorted digest per piece (function, static and debug name, keyed by instance), the
hit would be a merge of two sorted arrays at memory bandwidth, about 35 µs on six cores. That is
a request to codegen (`fastcompile-sol-premap 1`).

### Mapping the image at start

The work that can't be skipped is one fixed map per segment of the image, about four: code,
read-only data, data and thread-local templates. Each segment's pages are already resident in
the image's memory object.

\[ T_{\text{map}} \approx 4 \cdot 2\ \mu s \approx 8\ \mu s. \]

`Memory::maps` gives exactly one `FixedMap` per segment, into a list allocated once at its
length, which is the bound. Pages the run touches then fault in as minor faults, one per 16 KiB
page touched. That cost belongs to blaze-inproc's `CodeMemory`, and with huge pages it would be
one fault per 2 MiB.

The image is opened once per key per machine, not per run. That costs one copy of the metadata
(the 32 MB table plus about 6 MB of names, about 4 ms) and a fill of the memory object (85 MB, about 8.5 ms from one core).
An opened `Ready` keeps only the metadata, so a machine with 8 graphs open holds 8 tables, not
8 images.

### The cold prelink

The cold prelink runs once per image key, when dependencies change. It is bound by the bytes
written and the relocations applied:

\[ T_{\text{prelink}} \approx \frac{W}{B} + \frac{W}{16\ \text{KiB}} \cdot 0.5\ \mu s \cdot
\frac{1}{c} + \frac{5\ \text{ns} \cdot R}{c} \approx 0.7 + 0.4 + 1.2 \approx 2.3\ \text{ms at }
c = 6, \]

where \(B \approx 120\) GB/s is the all-core write bandwidth. On top of that, premap's own work
that can't be skipped is:

- the closure check, one probe of a hashed set per distinct target:
  \(800\,000 \cdot 10\ \text{ns} / c \approx 1.3\) ms;
- the table, one slot write per entry: about 1 to 2 ms on one core;
- BLAKE3 over the image for the store, all cores: about 1 ms.

So the bound for the whole cold path is about 6 ms.

**Against mold and wild.** mold links Chromium (about 2 GB of output) in about 2 s on 16
cores, about 1 GB/s end to end. At that rate an 85 MB image takes about 85 ms, and wild is
roughly 1.5 to 2 times faster on debug links. Both start from object files on disk: parsing,
symbol resolution by string and section merging. Here the pieces are already in memory, keyed
by instance key, so the bound is set by bytes and relocations alone, an order of magnitude
under a whole-program linker.

**What the code does.** `image_part` checks each function's and static's relocation targets
(`Relocs::targets()`, each target once) against open-addressed sets on the frame, in parallel.
Before, it checked every row with a binary search, about 80 ms. The prelink's placement check
is one table probe per piece; before, it sorted and binary-searched. `encode` writes the image
once, copying the segments in parallel 1 MiB pieces into a zeroed buffer. `Job::make` opens the
image from the bytes and moves them into the store, with no clone (before, it copied all 85 MB).

Still off the bound: the segments' bytes are written three times. blaze-link writes them into
its own buffers, `encode` copies them into the image, and the memory object is filled from that.
That is about 2.5 times the write term, a few milliseconds. Writing once needs blaze-link to
write its segments into a buffer premap gives it (`fastcompile-sol-premap 2`). A store hit
opens the image from the store's own bytes (`Shelf::get_image` lends them), so it copies only
the metadata and the memory object's fill.
