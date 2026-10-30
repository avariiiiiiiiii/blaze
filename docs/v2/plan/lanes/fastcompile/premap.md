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

- `GraphKey` (`"blaze v2 premap graph 1"`): for every dependency unit, its package id, target,
  features, cfgs and rustflags. Then:
  - the toolchain (`settings::Toolchain`: the nightly's and blaze's code identity, triple, arch,
    cpu, target features, backend versions, frame pointers, relocation model);
  - every codegen settings id the image's functions were compiled under;
  - blaze-link's code identity;
  - the output it links for (format, arch, kind and entry or name and exports, interpreter,
    platform and versions, incremental, debug info, live, tags, stack, Windows subsystem);
  - the base address;
  - the run loader's thread-local bias;
  - every native input of the link line: content id, kind (object, archive and whether whole,
    shared library and whether as-needed) and place on the line;
  - the span side tables positions are filled from.
- `ImageKey` (`"blaze v2 premap image 1"`, the store's key and the memory object's name): the
  graph key, then every function by instance key and code hash, and every static by item,
  instance and definition (value, relocations, alignment, mutability, thread-local, section,
  the data it points at). Then every global name the image defines and what it is, and every
  debug name.

Images of one graph differ only in which instances they hold: a build that needs an instance
the graph's image lacks links it with the workspace this time (`spilled`), and the image of its
own key is made in the background.

Tested: `graph_key_changes_with_every_input` changes each of the 35 inputs alone and checks
every key differs from all the others; `image_key_changes_with_every_instance` does the same
for the image key's 11; `graph_key_ignores_order_of_sets` checks order-free lists are and
rustflags aren't.

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
- `workspace_part`, given an image's table, takes everything the table lacks. Every piece is in
  exactly one of the two, so every function and static has one address.

## The prelink and the image

`prelink.rs`, `image.rs`, `table.rs`. blaze-link links the image as a shared output
(`OutputKind::Shared`, never incremental, never live) at the fixed base `0x2000_0000_0000`. That
is 32 TiB, inside every 47-bit user address space and far from where every loader puts
executables, libraries, stacks and heaps. It goes through a hook (`Prelink`,
`fastcompile-premap 1`) whose stage runs in parallel, as any full link does. What comes back is
checked against what was asked:

- every function and static is placed;
- segments are aligned to the target's mapping granule (16 KiB on Apple silicon, 64 KiB on
  other AArch64 and on Windows, 4 KiB otherwise), in order and apart;
- every bind cell is an aligned word of a writable segment;
- every init piece, thread-local template, unwind range and Mach-O thread-local descriptor lies
  inside the image.

It is then encoded as one value: a 240-byte header (magic, version, format, arch, key, base,
granule, directory) and fixed-layout records. `View::parse` checks every offset, length, name
and address against the bytes and the key before anything reads them.

The symbol table is open-addressed, at most half full, in 64-byte slots: id, value, size,
line order, kind, strength. A slot is found by the first 8 bytes of its id: the instance key,
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
its memory object filled), shared by every view and run, never one per view. `dev::plan`:

1. splits the build and keys its image;
2. if that image is open, uses it;
3. else uses the newest image of its graph and links the spilled instances with the workspace;
   if anything spilled, the image of its key is claimed and made in the background;
4. with no image of the graph, makes one (from the store if it is there, else by the prelink).
   Another build meanwhile waits rather than making it twice.

A finished image replaces its graph's previous one, which closes when no run maps it. At most 8
graphs are kept (a machine's branches and toolchains), the least recently used closed first. A
failed image fails again for its key: no whole link takes its place. Tested:
`an_image_is_made_once_per_key_and_again_only_when_a_build_needs_more` and
`a_failed_image_fails_again_and_never_falls_back`.

## Limits, each an error naming it

- AArch64 `BL` reaches ±128 MiB. A workspace call into the image across more than that needs
  the workspace image placed within reach (it is placed right after the image) or a veneer from
  blaze-link. Without either, blaze-link's range check fails the link; nothing is silently
  redirected.
- A TpOff reference from the image to a library's thread-local is refused (above).
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
