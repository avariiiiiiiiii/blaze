# Requests from lane store

## 1. Workspace dependencies `zstd` and `lz4_flex`

- What: add to the root `Cargo.toml`'s `[workspace.dependencies]`:
  - `zstd = { version = "0.13.3", default-features = false, features = ["zdict_builder", "zstdmt"] }`.
    zstd 0.13.3 is already in `Cargo.lock` through `blaze-store`; `zstdmt` builds zstd-sys
    with its thread pool.
  - `lz4_flex = { version = "0.11", default-features = false, features = ["std", "safe-decode"] }`.
    This is new to the lock, so it needs a download. `safe-decode` is there because a rotted
    pack must decode to an error, never to an out-of-bounds read; encoding keeps the faster
    path (one build for every machine, so its bytes are the same everywhere).
- Where: root `Cargo.toml`. `crates/blaze-cas/Cargo.toml` already says `zstd.workspace = true`
  and `lz4_flex.workspace = true`.
- Why:
  - `zdict_builder`: cold values (debug info, build-script runs) are zstd with a dictionary per kind (docs/v2/plan/lanes/store.md), and this feature trains the dictionaries.
  - `zstdmt`: long cold values use long-distance matching on a fixed 4 workers (plan/hardware.md). Without it zstd refuses `NbWorkers`, which makes the store stop.
  - LZ4: chunks of hot outputs, where decompression speed matters.

> applied: root `[workspace.dependencies]`: `zstd = { version = "0.13.3", default-features = false, features = ["zdict_builder", "zstdmt"] }` and `lz4_flex = { version = "0.11", default-features = false, features = ["std", "safe-decode"] }`, each with its reason. The workspace's other zstd users (blaze-format) use nothing of the dropped defaults; blaze-cargo, blaze-engine and blaze-store name zstd directly. `lz4_flex` is a lock addition.

## 2. At integration: delete v1's store and remote

- What: delete `crates/blaze-store/` and `crates/blaze-cargo/src/remote.rs` with
  `crates/blaze-cargo/src/remote/` (v1's S3 / HTTPS / gateway pack sync, signed lists, keeper).
  They are mine, but deleting them now breaks v1's daemon and `blaze-engine cache ...`
  commands, which are the coordinator's until integration.
- Where: the paths above; their users in `crates/blaze-cargo/` (`BLAZE_REMOTE*`, `cache
  push|pull|stats|gc|verify`) and `crates/blaze-engine` (`blaze_store::*`, `BLAZE_SOURCES`).
- Why: docs/v2/plan/reference/crates.md ("`blaze-store` replaced by `blaze-cas`, deleted at integration");
  docs/v2/plan/lanes/store.md: the remote is only a read-through tier for machines outside the pool,
  never the VM's, superseding the per-job remote naming (#1481). The v2 tier is
  `blaze_cas::remote::{Client, Server, Answer}`: the VM daemon runs a `Server` on a Unix
  socket (or a loopback port forwarded over SSH); a laptop's daemon holds a `Client` and calls
  `Store::fetch(key, &mut client, effects)` when a get misses, routing completions by
  `client.owns(tag)` / `store.owns(tag)` and handing each answer `client.complete` returns to
  `store.complete`. With v1's `Identity` gone, no key hashes all of blaze's sources (#1439):
  `blaze_cas::Key::of(schema, producer, inputs)` keys a value by its schema and the code that
  made it.

> forwarded to coordinator: deletion at integration (in the requests report's integration list).

## 3. For core: one daemon a store directory

- What: an exclusive lock on a file, as an effect (e.g. `Effect::Lock { file, tag }`
  completing `Done::Locked` or `Done::Failed` naming who holds it, released when the driver
  ends), or the daemon taking such a lock on its state directory before it opens a `Store`.
- Where: `crates/blaze-core` (core), or the daemon (coordinator) at integration.
- Why: a store has exactly one writer (#686: only the daemon holds the store). Its commit cuts
  each pack back to the length the head names before appending; a second writer on the same
  directory would cut the first one's appends. `blaze-cas` cannot take a lock through the
  effects it has, so today nothing stops a second daemon; it must stop with an error instead.

> forwarded to core: `crates/blaze-core` has no lock effect yet.

## 4. For core: what the store needs of `Effect::Map`

- What: on the threads and io_uring drivers (still `unimplemented!` there): map the file's
  length at the time of the call, read-only, shared; a file that is appended to afterwards
  keeps its old mapping valid (the store maps a pack again after each commit and reads the old
  mapping only below the length it had). A zero-length file never comes up (packs are at least
  16 bytes, index tables at least 64 KiB).
- Why: the store reads packs, index tables and large values in place through these mappings.

> applied: by core (`Effect::Map` in `threads.rs` and `uring.rs` through `fs::map`).

## 5. For format: batched BLAKE3 (`ContentId::of_many`) and NEON

- What:
  - In `crates/blaze-ids`: `ContentId::of_many(domain: &'static str, inputs: &[&[u8]]) ->
    Vec<ContentId>`. It must give exactly `inputs.iter().map(|it| ContentId::of(domain, it))`,
    but hash many small inputs in one SIMD pass: 16 at a time with AVX-512, 4 with NEON. The
    `blake3` crate only has this internally (`hash_many`) and does not export it.
  - In the root `Cargo.toml`: add the `neon` feature to `blake3`. On aarch64 (Graviton,
    Apple) NEON is chosen at compile time, not at run time, and has no effect elsewhere.
    AVX-512 on x86_64 is already detected at run time.
- Where: `crates/blaze-ids` (format) and the root `Cargo.toml` (coordinator).
- Why: plan/hardware.md, "BLAKE3 ... one batched call per many small inputs". The store
  would batch:
  - in `Store::verify`, which hashes every record;
  - at open, the record headers it reads past the index;
  - in `put` of a chunked output, its chunks;
  - on the remote server, many small answers.

  Until it exists, the store hashes one input at a time, which is bit-identical.

> applied: `blake3`'s `neon` feature in the root `Cargo.toml`.
> forwarded to format: `ContentId::of_many` (`crates/blaze-ids`) is not there yet.

## 6. For core: what the store's pack writes need of the io_uring driver

- What:
  - Open pack files (`pack-*` in a store directory) with `O_DIRECT` on Linux and write their
    `Effect::Append`s from registered, 4 KiB-aligned buffers. Every append the store asks
    for starts and ends on a 4 KiB block: each commit pads each pack to a block (format 2).
  - Keep `Effect::Truncate` and `Effect::Sync` as they are; with `O_DIRECT`, `Sync` is still
    needed for the file's length.
  - Link each pack's `Truncate -> Append -> Sync` as io_uring linked submissions; the store
    already orders them and waits for each.
  - Use SQPOLL with registered files.
  - Use multishot `accept` and `recv` for the remote's `Listen` and `Recv`.
  - Map packs and index tables with `MADV_HUGEPAGE` (where the file system supports
    huge pages for files) and `MADV_WILLNEED` for index tables. Map them on the NUMA node of
    the worker that serves gets.
- Where: `crates/blaze-core`'s drivers.
- Why: plan/hardware.md's IO and memory rows. The store is sans-IO, so it can only ask for
  these through its effects. A buffer that is not aligned (sim, threads driver) must still
  work there; under io_uring an unaligned `Append` to a pack is a store bug, and the driver
  should fail it with that name, not quietly write it buffered.

> forwarded to core

## Column codecs belong to the value encoding (plan/design/data.md)

- Who: format.
- What: plan/design/data.md's compression paragraph puts frame-of-reference bit-packing, run
  lengths and deltas over columns with "format, store". The store sees values as opaque bytes,
  so it cannot pick a codec per column. Apply those codecs in the value encoding, before `put`.
  The store's part is the last stage: it applies zstd (with a dictionary per kind) and picks the
  hot, warm or cold form by read recency (`Store::settle`).
- Why: a column codec chosen after the bytes are opaque would have to guess the layout. Applied
  in the encoding, it round-trips exactly by construction, and zstd still runs over the result.

> forwarded to format

## Whole-value tier encodings, as `blaze_cas::Columns` (plan/design/data.md, "Compression follows the columns")

- Who: format, with the daemon wiring them in.
- What: `crates/blaze-cas` has `pub trait Columns { schema; warm; hot_from_warm; cold;
  hot_from_cold }` and `Store::encode_columns(Arc<dyn Columns>)`. The store moves records
  hot to warm to cold by read recency, and back to hot when they are read in two periods
  running. It decodes and compares every encoding before keeping it. A value takes part when it
  is a `blaze_format` buffer whose header names a schema the store was given.
  - Format: a whole-buffer re-encoding for each persisted schema, schema-driven. Every
    `Slice<u32>`-like column of the buffer becomes a `WarmColumn` (warm) or an `encode_cold`
    column (cold), and the buffer is rewritten around them. Its warm form is a buffer of its
    own schema, so readers open it in place.
  - Because blaze-cas depends on blaze-format, the `Columns` impls live in a crate above both
    (the persist modules, or the daemon), which calls `encode_columns` right after
    `Store::open`. Readers that can read the warm form call `Store::get_in_place` and branch on
    `Heat`; every other reader keeps `Store::get`, which always gives the value as put.
- Why: plan/design/data.md assigns the column encodings to format and the tiers to the store.
  The store cannot find columns in opaque bytes, so this is where the two meet. Until a schema
  is registered, its values stay raw or go to whole-value zstd as opaque bytes. A record already
  in columns is refused, naming its schema, if its encoding is not given.

> forwarded to format: the `blaze_cas::Columns` impls per persisted schema.
> forwarded to cargo: the daemon calls `Store::encode_columns` right after `Store::open`.

## Vendor zstd-safe and mark its dictionaries `Send` and `Sync` (everything is ours)

- Who: whoever owns `vendor/` (coordinator).
- What: vendor `zstd-safe` 7.3 (and `zstd` 0.13 with it) into the workspace, and add
  `unsafe impl Send` and `unsafe impl Sync` for `CDict` and `DDict`, which zstd.h documents as
  shareable read-only between threads. `crates/blaze-cas/src/cold.rs` (`Prepared`) asserts this
  today with its own `unsafe impl`. Once the vendored crate says it, that impl goes.
- Why: the store digests each kind's dictionary once, not once per put and per read. Loading
  64 KiB of dictionary into a zstd context cost more than decompressing a small cold value.
  The digested dictionaries are shared by every reader thread.

## Put each crate version's values in its unit (#1298)

- Who: every caller of `Store::put`: blaze-codegen, blaze-usercode, blaze-expand,
  blaze-cargo's serve, and db's query results through the daemon.
- What: call `Store::unit(crate_version)` once per crate version, with the content ID the
  caller already names crate versions by: name, version and source. Then put that crate's
  values with `Store::put_in(unit, key, kind, bytes)`.
  - `Store::put` stays, for values no one crate version owns. Its values go in the shared unit.
  - Output chunks always go in the shared unit, which the store does itself.
- Why: the store is now one mapped file per crate version, which the lane's "Done when" asks
  for, but a value only lands in its crate's file when its caller says which crate it is.
  Until then every value is in the shared file, as before.
