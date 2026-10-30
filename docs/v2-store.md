---
status: plan
summary: blaze v2's store as written by the store lane: the files of a store directory, how a group commit survives a crash, hot and cold values, chunks, placing, verification, layers, and the remote tier.
date: 2026-10-29
---
# blaze v2: the store, as written

The design is [v2/store.md](v2/plan/lanes/store.md) (section 8); this is how `crates/blaze-cas` does it.
`Store` is a sans-IO state machine: it maps, appends, syncs and writes through
`blaze_core::Effect`s and is told how each went by `Store::complete`.

## On disk

One flat directory per store (`Config::dir`):

| file | what |
|---|---|
| `head` | what the store is: its generation, which index slot is live (with its length, count and CRC-32C), each pack with its committed and indexed length, the dictionaries, the files of their own. Ends with a CRC-32C. Written whole with `Effect::Write` (temporary file, sync, rename, directory sync). |
| `pack-<kind>-<n>` | appended to only: a 16-byte header (magic, kind), then records. Every commit ends it on a 4 KiB block, the rest of the block zeros, so every append can be written with `O_DIRECT`. A pack takes no appends once it is 1 GiB. |
| `index-0`, `index-1` | the two index slots: an open-addressed table on keys, written whole into the slot the head does not name. |
| `file-<content>` | a hot value of 4 MiB or more (not an output), a file of its own: mapped on its own, placed by cloning. |

Packs sit next to the head so the directory sync of the head's write makes a new pack's name
durable with it.

A **record** is a 96-byte header (magic, kind, form, a CRC-32C of the header, dictionary,
stored and raw lengths, key, content hash), the payload, and zeros to the next 16 bytes. Its
form is `Raw` (read in place), `Zstd`, `Lz4` (a chunk), `Chunked` (a list of chunk keys and
lengths), `External` (the value is `file-<content>`) or `Dictionary`. The store's format is 2. `Entry::content` is BLAKE3 of the
value's bytes under `"blaze v2 store value 1"`.

An **index slot** is 64 bytes, one cache line: the key, the pack, kind, form, offset, stored and
raw length. Linear probing from the key's first 8 bytes (a BLAKE3 hash is uniform), at most 5/8
full: a lookup is one or two lines of the table and one of the pack.

## A group commit (#1298)

`put` only appends to memory: the value is readable at once and has a ticket. `flush` starts one
commit for everything put since the last:

1. each pack written to is cut back to its committed length (`Truncate`), then appended to
   (`Append`), then synced (`Sync`, asked only once the append has completed);
2. new files of their own are written (`Write`); and once the records the index table does
   not hold reach 65,536, a new table of all of them goes into the inactive slot (`Write`);
3. once all of those are durable, the head is written, naming the new lengths and slot.

Before step 1 each pack written to is padded with zeros to the next 4 KiB block, so the append
starts and ends on a block; v2/requests/store.md asks the io_uring driver to write packs with
`O_DIRECT` from registered buffers.

The head's write is the flip: a crash before it leaves the old head, naming only bytes that were
whole before; after it, everything it names is durable. Bytes past a pack's committed length
(a torn append) are never read and are cut by the next commit. Any IO failure fails the whole
commit, which starts again from the committed lengths; eight failures in a row break the store
(`Store::broken`), which then serves what it has mapped and makes nothing durable. 77,481 puts
are one commit: a few large appends, a sync per pack, one head.

After the flip the store maps each grown pack again and drops the bytes it held in memory that
the new mapping covers; a mapping is never read past the length it was asked for.

## Opening (#686)

The store reads `head`, maps the live index slot, every pack and every file of its own, then
checks the index table's CRC-32C and reads the headers of the records between each pack's
indexed and committed lengths into an in-memory delta, stepping over the zeros that end a
commit. Nothing else is read: the index is the mapped table. Only the daemon holds a
store (one writer: the commit cuts packs back to their committed length, so two writers would
corrupt each other; v2/requests/store.md asks for the lock that enforces it).

A head of another format, a pack shorter than its head says, an index that does not match its
CRC, a record header in the committed range that does not check, or anything but zeros where a
commit's padding is stops the store, naming the file. Nothing is read as
another format and nothing is skipped.

## Hot and cold

Query results, compiled code, outputs and the image are **hot**: kept raw and read in place. Debug
info and build-script runs are **cold**: read rarely, kept zstd (level 6) with a dictionary of
their kind. A cold kind's first values are kept raw; once 256 of them (2 MiB) are there, a
64 KiB dictionary is trained on them, written as a record and listed in the head, and the kind's
values from then on are compressed with it. A training that fails is in `Store::problems` and is
tried again on twice the samples. A cold value of 8 MiB or more gets no dictionary but zstd's
long-distance matching over a 128 MiB window, on 4 threads: a fixed count, so the bytes are the
same on every machine. zstd refusing a value stops the store; it is never kept another way. A
compressed value is decompressed on read and held until the store is next changed.

## Chunks and placing

An output of 256 KiB or more is split by content (FastCDC, 16/64/256 KiB, a fixed gear table):
each chunk is a value keyed by the hash of its bytes, kept once however many outputs hold it,
and the output a list of them. A chunk is kept in LZ4 when that saves an eighth of it, raw
otherwise: a chunk is copied out when its output is assembled, so decompressing it costs about
the copy. Two builds of a binary that differ in a few functions share
all but the chunks around the differences.

`Store::place` puts a value at a path outside the store. A file of its own is cloned
(`Effect::Clone`: `FICLONE`, `clonefile`), after removing what is at the path; anything else,
chunked outputs included, is written whole, copied with non-temporal stores from 4 MiB on (the
copy goes to the kernel, not back to the store). The form decides, never a try: where blocks cannot
be shared the clone fails, naming the file system, and nothing copies instead.

## Verified

`get` checks the record header (its CRC-32C and its key), then hashes the value's bytes against
its content hash before handing them out, once a record (the bytes of a mapping or of memory at
a place never change). A chunked value checks each chunk and the whole. A value that fails is
`Corrupt` and never served; `verify` checks every value afresh, with the index tables' CRCs and
the dictionaries. CRC-32C finds torn and rotted headers and tables; what a value's bytes are
is always checked by BLAKE3. Places are `FileName`s, which cannot leave the driver's root, and `place`
refuses a path in the store.

## Keys (#1439, #1480)

A key is the caller's: the store adds nothing to it, so no key hashes all of blaze's sources and
a release that changes nothing a value depends on keeps it. `Key::of(schema, producer, inputs)`
is the one way to make one: the schema of the value, the content ID of the code that made it
(its own dependency closure), and every input. A put of a key with other bytes than it keeps is a
`Mismatch`, which is a key that misses an input. Build-script runs are keyed by their traced
inputs (`blaze-usercode`'s `RunKey`), which take in the C compiler they ran.

## Layers

`Config::layers` are read-only stores searched after the store's own: the shared base layer of
public dependencies. They are opened as the store is and never written; a put of a key a layer
has is durable at once. A layer that is not there stops the store.

## The remote tier

`remote::Server` serves a VM daemon's store over a Unix socket or a loopback port (forwarded over
SSH); `remote::Client` is a laptop daemon's way to it. `Store::fetch` asks; the answer is checked
against its content hash and put like any value. The channel is what is trusted for the key; the
bytes are checked. It is never on the VM's hot path: the VM's builds read their own store.

## Hardware (v2 plan section 16, plan/hardware.md)

Each hot loop picks its path at run time by CPU feature, and every path gives the same bits, so
which one runs is dispatch, never a fallback:

| loop | paths |
|---|---|
| CRC-32C (`crc.rs`) | SSE4.2 `crc32` or ARMv8 CRC, three streams for long inputs; tables elsewhere |
| FastCDC gear hash (`chunk.rs`) | AVX-512 (8 positions a step: gather, a shift-and-add scan, a lane test) or NEON; scalar elsewhere |
| copies to the kernel or the network (`copy.rs`) | non-temporal stores (AVX-512, SSE2, `stnp`) from 4 MiB on |
| BLAKE3 | the `blake3` crate's run-time AVX-512 / AVX2 / SSE4.1; NEON and many-small-inputs batching are requests to format |

The differential tests run every path the test machine has against the portable one. What the
store asks of the drivers (`O_DIRECT`, registered buffers, SQPOLL, linked submissions, huge pages
and NUMA-local mappings) is in v2/requests/store.md; threads and pinning are core's.

## Not done here

- Eviction: nothing is removed from a store. The VM's disk is taken as unlimited (v2 README
  section 2); a store is removed whole.
- One file per crate version (#1298's makepad layout): `put` takes no crate, so packs are per
  kind; the commit is #1298's (append, one barrier, index into the inactive slot, head flip).
