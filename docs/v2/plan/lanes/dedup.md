---
status: plan
summary: lane dedup (2026-10-29): one implementation per primitive (bit sets, CRC32C, radix sort, LEB128, timing wheels, pools, arenas, alignment, file mapping, interners, and every copy-paste run the scan finds), in the crate that owns that kind of thing, every caller moved onto it and the copies deleted.
date: 2026-10-29
---
# dedup

Lanes wrote the same primitive several times. This lane picks the best implementation of
each (or writes one that beats them all), puts it in the one crate that owns that kind of
thing, gives it the union of what callers need, moves every caller to it and deletes the
copies. Its children's notes, with each part's speed of light, are in
[dedup/](dedup/).

## Where each kind of thing lives

| kind | crate | module |
|---|---|---|
| bit sets, bit matrices | `blaze-ids` | `blaze_ids::bits` |
| radix sort | `blaze-radix`, re-exported as `blaze_ids::radix` (already the one sort; it stays its own crate because `blaze-core` and `blaze-graph` use it without `blake3`) | `blaze_radix` |
| CRC32C and other checksums, seeded test RNGs | `blaze-ids` | `blaze_ids::crc32c`, `blaze_ids::rng` |
| LEB128 and varints | `blaze-format` | `blaze_format::leb` |
| timing wheels | `blaze-core` | `blaze_core::wheel` |
| file mapping | `blaze-core` | `blaze_core::fs` (its `Mapped`) |
| object pools, arenas, `align_up` and friends | `blaze-mem` | `blaze_mem::{pool, arena, align}` |
| interners and their hash-table probes | `intern` | `intern` |
| edit distance and other small text helpers | `stdx` | `stdx::edit_distance` |

Thread pools (`stdx`, `blaze-graph`) and process pools (`proc-macro-api`, `blaze-usercode`)
are not object pools: they hold running threads or children, not memory, and are compared
only with each other.

## Rules for every part

- A copy that differs on purpose (another invariant, a hot specialization) stays only with a
  comment saying why, under a different name.
- The shared implementation gets every test the copies had, plus a `proptest` differential
  against the simplest copy as the reference, kept as a test-only module until that copy is
  deleted (then the reference stays in the test, as the spec).
- A caller is edited only when every agent that owns it reports `STATUS: DONE` (the "free" rule
  of the mem lane's brief). Otherwise the exact edit goes to `v2/requests/<owner>.md` under
  `## dedup: <title>`, and the owner hears of it by tmux.

## Parts

| part | primitives |
|---|---|
| `dedup-bits` | bit sets |
| `dedup-codec` | CRC32C, LEB128 and varints, SHA-256 in build scripts, hasher and RNG helpers |
| `dedup-order` | radix sort copies, timing wheels |
| `dedup-mem` | pools, arenas, alignment |
| `dedup-map` | file mapping |
| `dedup-intern` | interners, hash-table probe copies |
| `dedup-macros` | `blaze-expand` against `blaze-macro-wasm`: proc-macro library, lexer, wire, host |
| `dedup-cursor` | the checked little-endian byte cursor and writer |
| `dedup-names` | the rustc-symbol path resolver in `blaze-engine` and `blaze-wasm` |
| `dedup-sim-front`, `dedup-sim-edge`, `dedup-sim-diag` | the semantic duplicate report's cross-crate groups (`tools/similar`), by area |
| `dedup-sim-rest` | the 20 cross-crate groups of 15+ lines no other part owned after the report's rerun on 2026-10-29 |
| `dedup-scan` | every other copy-paste run (diagnostics against `blaze-lints`, cargo crates against `blaze-units`, edit distance, test databases, code-identity hashing, small helpers) |
