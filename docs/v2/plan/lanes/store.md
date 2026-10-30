---
status: plan
summary: blaze v2: the store.
date: 2026-10-29
---
# blaze v2: store

Part of the [v2 plan](../../plan.md). Section numbers refer to the plan as first written.

## 8. Store

- One content-addressed store on the VM's disk, keyed by BLAKE3 of exact inputs (source,
  cfg, features, target, toolchain, the layout facts read). A key that misses an input is a bug.
- **Packs**: aligned zero-copy records, mmapped. Hot objects uncompressed; cold objects zstd
  with a dictionary per kind. Large outputs content-defined-chunked, so near-identical binaries
  across views share bytes. Outputs placed with reflink (Linux) or `clonefile` (macOS).
- **Index**: an mmapped open-addressed table on BLAKE3 keys; one or two cache misses a lookup.
- **Verified**: a read checks the content hash; archive and path entries can't escape their
  directory.
- **Remote** is a read-through tier for machines outside the pool (a laptop) only, never on the
  hot path of the VM. Supersedes the per-job, per-pin remote naming (#1481).

## Issues this lane takes in

- **#1439**: the store keys every value by a hash of all of blaze's sources, so any release
  turns CI cold. v2 keys a value by the schema and code of what produced it (section 5,
  Schema). (`v2-store`)
- **#686**: every process indexes the whole store (1.6-1.8 GB each, OOM at 117 GB). In v2 only
  the daemon holds the store, and its index is the mmapped table. (`v2-store`)
- **#1480**: two store inputs not hashed (unreadable sources skipped, the C compiler of build
  scripts). Traced inputs hash both. (`v2-usercode`, `v2-store`)
- **#1298 store as one mapped file per crate version**: append payloads, write the index into
  the inactive of two slots, flip the header; one barrier, not a sync per entry (anim cold
  wrote 77,481 entries serially). (`v2-store`)

## Done when

- one mapped file per crate version with the index flip; the mmapped BLAKE3 index
- hot, warm and cold tiers with background moves; every read verified
- keys by schema and producing code; the remote as an off-path tier
- nothing in the lane's paths matches `rg 'unimplemented!|todo!'`; tests for all of it are written; its report says `STATUS: DONE`
