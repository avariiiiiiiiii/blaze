---
status: plan
summary: lane dedup's speed of light: where each shared primitive's bound, its arithmetic and how the implementation reaches it are written.
date: 2026-10-29
---
# dedup: speed of light

Each shared primitive's bound from the hardware, its arithmetic and how the one implementation
comes within 10% of it are in the part's notes beside this file:

| primitive | shared implementation | bound in |
|---|---|---|
| bit sets, bit matrices, ranked and atomic bits | `blaze_ids::bits` | [bits.md](bits.md) (and `bits-db.md`, `bits-edge.md`, `bits-store.md`) |
| CRC32C, SHA-256, hasher forwarders | `blaze_ids::crc32c`, `blaze-sha256`, `blaze_ids::StdHash` | [codec.md](codec.md) |
| LEB128 | `blaze_format::leb` | [codec-leb.md](codec-leb.md) |
| seeded RNG | `blaze_ids::rng::SplitMix64` | [codec-rng.md](codec-rng.md) |
| byte cursor and writer | `blaze_format::cursor` | [cursor.md](cursor.md) |
| counting sort into CSR, timing wheel | `blaze_radix::Csr`, `blaze_core::wheel` | [order.md](order.md) |
| alignment | `blaze_mem::align` | [mem.md](mem.md) |
| read-only file mapping | `blaze_core::fs::Mapped` | [map.md](map.md) |
| SwissTable probe, lock-free `u32` index | `blaze_mem::swiss` | [intern.md](intern.md) |
| proc-macro bridge, lexer, wire | `blaze_expand::bridge` (requested) | [macros.md](macros.md) |
| edit distance, Windows command line, signal names, the rest of the scan | `stdx::edit_distance` (requested), `blaze_core::{create_process, signal}` | [scan.md](scan.md), `scan-*.md` |
