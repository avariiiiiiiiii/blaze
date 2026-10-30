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
  - `zdict_builder`: cold values (debug info, build-script runs) are zstd with a dictionary per kind (docs/v2/plan/lanes/store.md section 8), and this feature trains the dictionaries.
  - `zstdmt`: long cold values use long-distance matching on a fixed 4 workers (plan/hardware.md). Without it zstd refuses `NbWorkers`, which makes the store stop.
  - LZ4: chunks of hot outputs, where decompression speed matters.

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

## 3. For core: one daemon a store directory

- What: an exclusive lock on a file, as an effect (e.g. `Effect::Lock { file, tag }`
  completing `Done::Locked` or `Done::Failed` naming who holds it, released when the driver
  ends), or the daemon taking such a lock on its state directory before it opens a `Store`.
- Where: `crates/blaze-core` (core), or the daemon (coordinator) at integration.
- Why: a store has exactly one writer (#686: only the daemon holds the store). Its commit cuts
  each pack back to the length the head names before appending; a second writer on the same
  directory would cut the first one's appends. `blaze-cas` cannot take a lock through the
  effects it has, so today nothing stops a second daemon; it must stop with an error instead.

## 4. For core: what the store needs of `Effect::Map`

- What: on the threads and io_uring drivers (still `unimplemented!` there): map the file's
  length at the time of the call, read-only, shared; a file that is appended to afterwards
  keeps its old mapping valid (the store maps a pack again after each commit and reads the old
  mapping only below the length it had). A zero-length file never comes up (packs are at least
  16 bytes, index tables at least 64 KiB).
- Why: the store reads packs, index tables and large values in place through these mappings.

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
