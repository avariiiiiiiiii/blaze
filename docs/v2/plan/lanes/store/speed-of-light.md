---
status: plan
summary: lane store's speed of light: the bound each component's work and the hardware set, the arithmetic, and how the design reaches 90% of it.
date: 2026-10-29
---
# Lane store: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md). Each component has a section here, written by the
agent that owns it; each edits only its own section. The machine figures are those of
[link's speed of light](../link/speed-of-light.md#the-machine), unless a section says otherwise.

## The remote tier

The v1 remote sync in `crates/blaze-cargo/src/remote.rs` and `remote/` is deleted at integration
(`v2/requests/store.md` request 2), so it gets no bound. The v2 tier is `crates/blaze-cas/src/remote.rs`,
which belongs to agent `concurrency`. Its bound belongs to that agent, and is written here or in
`blaze-cas`'s notes. `speed-link-edge-remote`'s measurement of that tier, and the requests C1
to C8 (`v2/requests/concurrency.md`) and K1 and K2 (`v2/requests/core.md`), give the inputs.

The bound a fetch has, per byte and per hit:
- **A hit**: one round trip for a batch of lookups. With `FindMissingBlobs`-style batching, a
  build of 10,000 units asks once, not 10,000 times. Over 1 Gbit/s with 20 ms of RTT, the bound
  is one RTT (20 ms) plus 10,000 × 32 bytes of keys / 125 MB/s (2.6 ms).
- **Bytes**: the network's bandwidth, and one copy into the pack: 1 GiB over 10 Gbit/s is 0.86 s.
  The local copy (about 10 GB/s) and the hash (BLAKE3 at about 6 GB/s a core, on 4 cores 24 GB/s)
  are each under 5% of that.
- **What stands in the way** (from the measurement): today 5 copies and 2 hash passes per byte,
  one send per answer, and head-of-line blocking inside a batch. C2 to C7 and K2 bring that to
  2 copies and 1 hash pass. At 10 Gbit/s that is within 10% of the network bound, and at 100
  Gbit/s the copies are the bound: then K1 (zero-copy send) and K2 (receive into place) are
  what reach it.
