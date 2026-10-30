# Requests from lane store

## 1. Workspace dependency `zstd`

- What: add `zstd = { version = "0.13.3", default-features = false, features = ["zdict_builder"] }`
  to the root `Cargo.toml`'s `[workspace.dependencies]`. Already in `Cargo.lock` (0.13.3, through
  `blaze-store`), so the lock does not change.
- Where: root `Cargo.toml`; used by `crates/blaze-cas/Cargo.toml` (`zstd.workspace = true`,
  already written).
- Why: cold values (debug info, build-script runs) are zstd with a dictionary per kind
  (docs/v2/store.md section 8); `zdict_builder` trains the dictionaries.

## 2. At integration: delete v1's store and remote

- What: delete `crates/blaze-store/` and `crates/blaze-cargo/src/remote.rs` with
  `crates/blaze-cargo/src/remote/` (v1's S3 / HTTPS / gateway pack sync, signed lists, keeper).
  They are mine, but deleting them now breaks v1's daemon and `blaze-engine cache ...`
  commands, which are the coordinator's until integration.
- Where: the paths above; their users in `crates/blaze-cargo/` (`BLAZE_REMOTE*`, `cache
  push|pull|stats|gc|verify`) and `crates/blaze-engine` (`blaze_store::*`, `BLAZE_SOURCES`).
- Why: docs/v2/crates.md ("`blaze-store` replaced by `blaze-cas`, deleted at integration");
  docs/v2/store.md: the remote is only a read-through tier for machines outside the pool,
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
