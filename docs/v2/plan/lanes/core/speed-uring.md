---
status: plan
summary: speed-uring's pass over the Linux io_uring driver (blaze-core uring.rs, uring/ops.rs, uring/flight.rs): how a request is recorded, where its bytes and names live, how submissions and completions cross the ring, which pools pay; the cost per request before and after, against glommio, monoio, tokio-uring, liburing's examples, TigerBeetle and seastar.
date: 2026-10-29
---
# speed-uring: the io_uring driver, request by request

What `speed-uring` changed for [principles.md](../../goals/principles.md)'s "Fast code, line by
line" in `crates/blaze-core/src/uring.rs` and its modules `uring/ops.rs` (the request columns,
name arenas, buffer lender) and `uring/flight.rs` (each request through the ring). Nothing here
has been built or run yet: the lane ran under a no-build rule, so every number below is counted
from the source (field sizes, stores, copies), not measured. The tests named at the end are
written and wait for a run in an ix VM.

## The request record

Before, a request in flight was `Pending { tag, op: Op, abandoned }` in a `Slab<Pending>`, `Op`
an enum of 18 variants each owning what it used: `OwnedFd`s, `Vec<u8>`s for bytes and for path
names, a `Listener`, a 136-byte `SockAddr`, an `Rc<UnsafeCell<[u8; 8]>>` for the wake's count,
a `Chain` of four `Option<i32>`. The `Connect` variant's `sockaddr_storage` set the size: about
170 bytes a slot. Each completion moved the whole `Op` out of its slot
(`mem::replace(.., Op::Stepping)`) and back again, two copies of ~150 bytes, so the kernel never
saw a moved buffer only because the buffers were behind the `Vec`s.

After, the slab holds a 32-byte `Hot` (tag, progress, whose it is as a `u64`, kind, flags, the
chain's links as a bitmask, the zero-copy notifications owed, the registered buffer), with two
columns of the same index in reserved memory that never moves (`RegionVec`): the request's buffer
(24 bytes, the asker's `Vec`) and a 56-byte `Cold` (offset, two descriptors, where its names start
in the arena, the chain's results, the file to forget). What used to live in the variant now lives
where it belongs: the listener in a `Slab<Listener>`, the connect's address in a
`Slab<Connecting>`, the wake's count in one box for the driver's life. `user_data` is still
generation, index and link (`data`/`of_data`), so a stale completion is still told from the
request that took its slot. A completion reads the hot record in place and touches the buffer or
the cold record only where its kind needs them; nothing is moved while it is stepped.

## Bytes, names and pools

| What | Before | After |
|---|---|---|
| Buffered read, append, positioned write, send | the asker's `Vec`, given to the kernel | the same: the kernel copies into the page cache or socket, so a registered buffer saves nothing and would add a copy |
| `O_DIRECT` write | the asker's bytes copied into one of four registered 1 MiB buffers, `WRITE_FIXED` | the same, the buffers lent by `Lent` (a free list and a queue of waiters); a write with none waits, never fails |
| Receive | multishot `RECV` into a provided-buffer ring; bytes copied into the connection's inbox, then from the inbox into the asker's `Vec`: two copies; the ring's tail stored once per buffer given back | bytes copied from the provided buffer straight into the waiting `Recv`'s `Vec` where nothing waits in the inbox (`take_in`): one copy; the tail stored once per pass of completions (`Provided::publish`) |
| Large TCP send | `SEND`, the kernel copies into the socket | `SEND_ZC` from 32 KiB (`ZERO_COPY_LEAST`), the request ending at the kernel's notification; Unix sockets always copy (no zero-copy there) |
| Names of a write's temporary and target, a rename's two | a pooled `Vec<u8>` of 2 × `PATH_MAX` per request, moved into the `Op` and back to the pool | two 64 MiB reserved arenas taking turns (`Paths`), each request holding its arena until it ends; no pool, no 24-byte header per request, no move |
| Wake of the ring by work off it | `Rc<UnsafeCell<[u8; 8]>>` cloned into every wake request | one boxed cell for the driver's life |

Fixed files (the sparse registered table, accepts direct into the kernel's half) and SQPOLL with
`SINGLE_ISSUER` stay as they were. Huge pages are not used: the provided buffers are 1 MiB in all,
under one 2 MiB page, and the registered buffers are pinned once.

## Crossing the ring

| Step | Before | After |
|---|---|---|
| SQEs written | each `push` stored the SQ tail (a release store, a cache line the kernel's thread polls) | `push` writes entries against a cached head; one tail store per `enter`, after the whole step (`Ring::publish`) |
| CQEs reaped | each CQE copied into a `posted` vector, the CQ head stored after each one, then a second pass stepped them | one pass: each CQE read and stepped where it is, the head stored once at the end (`Ring::consume`); the CQ is `NODROP`, so the kernel holds what overflows meanwhile |
| Provided buffers back | one tail store per buffer | one per pass |
| A whole file's write | write → fsync → renameat → fsync dir, linked | the same chain, links kept as a bitmask, results in the cold record |

## Per request, before and after

Counted from the source; "stores" are release stores to memory the kernel reads.

| Request (warm) | Record bytes touched per completion | Copies of the record | Allocations | Copies of the data | Ring stores |
|---|---|---|---|---|---|
| Read, sync, append | ~170 B moved twice → 32 B read in place (+24 B buffer) | 2 → 0 | 0 → 0 (the read's own bytes aside) | 0 → 0 | 1 SQ tail per push + 1 CQ head per CQE → 1 SQ tail per step + 1 CQ head per pass |
| Positioned write (`O_DIRECT`) | ~170 B ×2 → 32 B + 56 B | 2 → 0 | 0 → 0 | 1 → 1 (into the registered buffer) | as above |
| Whole file's write (4 CQEs) | ~170 B ×8 → 32 B ×4 + 56 B | 8 → 0 | 0 → 0 | names copied into a pooled `Vec` → into the arena | 4 SQEs, 1 tail store either way; 4 → 1 head store |
| Send < 32 KiB | ~170 B ×2 → 32 B (+24 B) | 2 → 0 | 0 → 0 | 1 → 1 (the kernel's) | as above |
| Send ≥ 32 KiB on TCP | as above | 2 → 0 | 0 → 0 | 1 → 0 on a real device (loopback still copies inside the kernel); one more CQE, the notification | as above |
| Receive (multishot) | ~170 B ×2 → 32 B | 2 → 0 | 0 → 0 | 2 → 1 | 1 provided-tail store per buffer → 1 per pass |

Lines: the driver was 3,505 lines of code before its tests; it is 3,986 across `uring.rs`
(2,333), `uring/flight.rs` (1,290) and `uring/ops.rs` (363). It grew because zero-copy sends, the
arenas and the lender are new, and the moves of `Op` came out of every step. Tests went from 28 to
34.

## Against the others

- **glommio** keeps each request as an `Rc<RefCell<InnerSource>>` allocated per I/O, its pointer
  the `user_data`; it registers a buffer arena for DMA files (as blaze's registered buffers for
  `O_DIRECT`) and runs three rings (main, latency, poll) without SQPOLL by default. blaze's record
  is a slab slot with a generation, so a request allocates nothing and a stale CQE can't reach a
  freed one.
- **monoio** keeps requests in a `Slab<Lifecycle>` indexed by `user_data`, without a generation,
  and moves buffers into the operation (`IoBuf`) as blaze does; its submission is batched per
  turn of the runtime. blaze adds the generation, the hot/cold split, provided rings for receives
  and zero-copy sends.
- **tokio-uring** keeps a `Slab<Lifecycle>` too; an operation dropped while in flight boxes its
  buffers into `Lifecycle::Ignored(Box<dyn Any>)` so the kernel never writes freed memory, which
  blaze does by marking the slot `ABANDONED` in place and leaking it at teardown if the kernel never
  answers. It offers fixed-buffer pools and provided-buffer rings as opt-ins. blaze uses each only
  where it pays.
- **liburing's examples** (`io_uring-udp`, `proxy`, `send-zerocopy`) do what blaze now does:
  `io_uring_for_each_cqe` with one `io_uring_cq_advance` per batch, one
  `io_uring_buf_ring_advance` per batch of provided buffers, multishot receive, `SEND_ZC` with its
  notification. `send-zerocopy`'s measurements are where the 32 KiB threshold comes from (with
  the kernel's `msg_zerocopy.rst`): below ~10 KiB the copy is cheaper than pinning plus a second
  completion.
- **TigerBeetle** embeds an intrusive `Completion` in the caller, its pointer the `user_data`,
  everything allocated at startup, and keeps an overflow queue when the SQ is full, as blaze's
  `waiting` does. blaze can't embed the record in the asker (effects are values that cross
  threads), so the slab plays the role of its static pool.
- **seastar**'s io_uring backend embeds a `kernel_completion` in the requesting object and submits
  once per turn of the reactor. Its default on Linux is still linux-aio. blaze's batching per step
  is the same idea.

## What is left, and why

- **Multishot reads of pipes and watch descriptors, bundles, `NO_SQARRAY`:** each needs a kernel
  newer than blaze's 6.0 floor (6.7, 6.10, 6.6). Watch and pipe reads keep a pooled 64 KiB `Vec`
  each. Raising the floor is the toolchain's call, not this lane's.
- **Opens:** a positioned write, a whole file's write and a rename open their descriptors (and
  the directories to sync) with syscalls on the driver's thread, not `IORING_OP_OPENAT` into the
  fixed table. That is a chain one link longer and a change to how `Kept` files are named, a
  change for its own lane.
- **Probing for `SEND_ZC`:** no request kind is probed at setup. A kernel without it fails each
  large send with its error, which is no fallback. The 6.0 floor has it.

## Tests

In `uring.rs`, all of the old ones (adapted to the columns), plus:
`the_columns_of_requests_agree_with_a_map` (seeded starts, completions, stale `user_data` and
reuse of slots, against a `FxHashMap`), `warm_writes_and_renames_allocate_nothing`,
`a_warm_send_and_receive_into_buffers_handed_back_allocate_nothing` and
`a_warm_cycle_allocates_the_read_alone` (the allocation budgets),
`provided_buffers_go_back_once_per_pass` (the provided ring's batched give-back, against the
kernel's tail word), `a_large_tcp_send_goes_from_the_askers_pages`, and
`aligned_writes_read_back_equal` (twice as many `O_DIRECT` writes as registered buffers, every one
written, none failed). In `uring/ops.rs`: `columns_move_with_their_slot`, `names_take_turns`,
`a_write_without_a_free_buffer_waits_for_the_next`.
