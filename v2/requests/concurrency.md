# Requests to `concurrency` (owner of `crates/blaze-cas/`)

## From speed-link-edge-remote: the remote tier (2026-10-29)

Paths: `crates/blaze-cas/src/remote.rs`, and `Store::take_fetched`/`put` in
`crates/blaze-cas/src/lib.rs`. Before and after numbers:
`docs/v2/plan/lanes/store/speed-link-edge-remote.md`. Nothing outside `blaze-cas` calls
`blaze_cas::remote` yet, so the signature changes below break no caller. C1 is a memory bound,
so do it even if you skip the rest.

> forwarded to concurrency: C1-C8 below.

### C1. Server: bound the answers queued from one batch (remote.rs `Server::serve`, ~line 677)

- Now: `serve` answers every whole ask in `peer.buffer`. `QUEUED_MOST` is only checked in
  `pump` before the next `Recv`. One receive of `SERVER_RECV` (1024 asks) for large values
  queues up to 1024 frames: up to 1 TiB in memory.
- Change:
  - loop `while peer.queued <= QUEUED_MOST && peer.buffer.len() - start >= ASK_LEN`;
  - in `complete`'s `Done::Sent` branch, call `self.serve(conn, store, &mut corrupt, effects)`
    in place of `self.pump(conn, effects)`, so the asks left in the buffer are answered as sends
    drain (`serve` ends in `pump`; `pump` already holds the `Recv` back while over the bound).
- Test: put 4 values of 40 MiB each, send 4 asks in one receive, and check that at most two
  frames are queued before the first `Done::Sent`. All 4 still arrive, in order.

> forwarded to concurrency: `crates/blaze-cas/` is lane concurrency's now (this file's title); lane concurrency is still running.

### C2. Server: one `Vec` and one send per batch of small answers

- Now: `answer_frame` allocates a `Vec` per answer, and `pump` sends each alone. 100 missing
  answers cost 100 allocations and 100 syscalls of 49 bytes each.
- Change:
  - Replace `answer_frame`/`framed` with
    `fn write_frame(out: &mut Vec<u8>, write: impl FnOnce(&mut Vec<u8>))`. It pushes an 8-byte
    placeholder, writes, then patches the length at `start..start + 8`.
  - In `serve`, for each ask, check `store.get(&key)`'s length first:
    - if it is under `COALESCE = 1 << 18`, append the frame to `peer.out.back_mut()` when that
      exists and its `len()` is under `1 << 20`, else to a new `Vec` pushed back;
    - a longer value gets a `Vec` of its own, reserved to `8 + VALUE_HEADER + len`.
  - Entries of `peer.out` are never in flight (`pump` pops the one it sends), so appending to
    `back_mut()` is safe. `queued += frame_len` as now.
- Test: 100 asks in one receive (half missing, half 1 KiB values) give exactly one
  `Effect::Send`. It decodes to 100 frames in ask order.

> forwarded to concurrency: `crates/blaze-cas/` is lane concurrency's now (this file's title); lane concurrency is still running.

### C3. Client: one send per turn of fetches (`impl Remote for Client`, ~line 503)

- Now: `fetch` pumps at once, so the first ask of a turn is sent alone. The asks of the turn's
  other fetches wait for that `Done::Sent`: a second send, and up to one more round trip.
- Change: in `fetch`, when `Link::Up(up)` has `up.sending == Some(tag)`, look for that send still
  unsubmitted, `effects.iter_mut().rev().find_map(|e| match e { Effect::Send { tag: t, bytes, .. } if *t == tag => Some(bytes), _ => None })`.
  If it is found, `write_ask(bytes, key)` into it. Otherwise write to `self.out` as now.
  `waiting.push_back` as now.
- Test: 24 fetches with one `effects` `Vec` give exactly one `Effect::Send` of 24 × 40 bytes.

> forwarded to concurrency: `crates/blaze-cas/` is lane concurrency's now (this file's title); lane concurrency is still running.

### C4. Client: adopt the driver's receive `Vec` (`Client::complete`, `Role::Recv`, ~line 381)

- Change: `Done::Read(bytes)` →
  `if up.buffer.is_empty() { up.buffer = bytes } else { up.buffer.extend_from_slice(&bytes) }`.
- Saves a copy of every answer that arrives in one receive, and of the first receive of a long
  one. The existing split-at-N tests cover it.

> forwarded to concurrency: `crates/blaze-cas/` is lane concurrency's now (this file's title); lane concurrency is still running.

### C5. Client: hand a long first frame on without a copy (`take_answers`, ~line 439)

- Now: `Done::Read(body.to_vec())` copies every value once more.
- Change:
  - `Done::Read` carries the whole frame, length prefix included (see C6).
  - When the frame starts the buffer (`start == 0`) and `8 + len >= CLIENT_RECV`, hand it on
    without a copy: if it is the whole buffer, `std::mem::take(&mut up.buffer)`; otherwise
    `let rest = up.buffer.split_off(8 + len); std::mem::replace(&mut up.buffer, rest)` (this
    copies only the bytes after the frame).
  - Other frames keep `rest[..8 + len].to_vec()`. Small frames that way avoid `split_off`'s
    quadratic copying.
  - Restructure the loop so the parse's borrow of `up.buffer` ends before the take: parse, keep
    `(tag, len)`, then move.
- Test: the 10 MiB answer test checks that the completion's `Vec` pointer equals the buffer's
  before the take.

> forwarded to concurrency: `crates/blaze-cas/` is lane concurrency's now (this file's title); lane concurrency is still running.

### C6. Store: one hash and no decode copy per fetched value (lib.rs `take_fetched`, ~line 2160; `put`, ~line 781)

- Now: `Answer::decode` copies the value (`bytes.to_vec()`). `take_fetched` hashes it with
  `content_of`, and `put` hashes it again.
- Change:
  - Make `remote::parse` `pub(crate)`. In `take_fetched`, parse `&bytes[8..]` in place (C5's
    frame), so the value is `&bytes[at..]` with `at = 8 + VALUE_HEADER`. Check the key, then
    `content_of` once.
  - Split `put` into
    `pub fn put(&mut self, key, kind, bytes: Vec<u8>)` (computes `content_of`, then calls the
    new function) and
    `fn put_hashed(&mut self, key: Key, kind: Kind, content: ContentId, mut bytes: Vec<u8>, at: usize) -> Result<Ticket, Mismatch>`.
    Its body is today's `put` body over `let value = &bytes[at..]`.
  - Only the `External` branch keeps the `Vec` (`memory: Some(bytes)`); it does
    `bytes.drain(..at)` first, a memmove of a value of 4 MiB or more, done once.
  - `take_fetched` calls `put_hashed(key, kind, content, bytes, at)`. `Answer::decode` stays
    public for tests.
- Test: the existing fetch tests (kept, bad hash, missing, I/O failure). Add a fetched External
  value (4 MiB or more), read back the same after a reopen.

> forwarded to concurrency: `crates/blaze-cas/` is lane concurrency's now (this file's title); lane concurrency is still running.

### C7. Wire version 2: answers in any order within a batch (no head-of-line blocking)

- Now: answers come in ask order, so a large value delays every answer asked after it.
- Change:
  - `VERSION = 2` (ask and answer). Version 2 means the server may answer a batch's asks in any
    order.
  - The client's `waiting` becomes `crate::ByHash<Key, Tag>` (a child module can name the
    parent's private alias; no new dependency). `Store::fetch` already refuses a second fetch
    of a key it is fetching, but two stores may share one client. So a fetch of a key already
    waiting is not asked again: it goes in `again: Vec<(Key, Tag)>`, and when the answer comes
    it gets a copy of the frame. An answer is matched by its key, and one for a key not waiting
    fails the connection as now.
  - The server's `serve` (with C2) writes missing and coalesced small answers before the frames
    of long values in the same batch.
  - The version check refuses a version 1 peer with the existing message. Both sides ship in
    one binary, but a laptop and its VM may run different blaze versions.
- Test: ask for a 10 MiB value, then 3 small ones in one send. The small answers complete
  before the large one. An answer for an unasked key fails the connection.

> forwarded to concurrency: `crates/blaze-cas/` is lane concurrency's now (this file's title); lane concurrency is still running.

### C8. `Client::complete` and `close` append to a caller's `Vec`

- Change: `complete(&mut self, completion, effects, answers: &mut Vec<Completion>)`, and the
  same for `close`. This drops an allocation per completion that answers something. The
  callers are the tests only.

> forwarded to concurrency: `crates/blaze-cas/` is lane concurrency's now (this file's title); lane concurrency is still running.

### Still needs core

`Effect::SendMapped` and `Effect::RecvInto` are requested in `v2/requests/core.md` (K1, K2).
- With K1, the server sends a `Form::Raw` record's bytes from the pack mapping: zero copies in
  user space.
- With K2, a long value lands with one copy across receives.

> forwarded to core: K1 and K2 (`v2/requests/core.md`).

## From safety-audit (2026-10-29, safety-audit-edge)

The workspace is about to deny `unsafe_op_in_unsafe_fn`, `clippy::undocumented_unsafe_blocks`
and `clippy::multiple_unsafe_ops_per_block`. Each finding below is comments, `# Safety` docs or
block splits only, in the original evaluation order; nothing changes what the code does.
Pointer-taking `std::arch` intrinsics (`_mm512_loadu_si512`, `_mm512_stream_si512`,
`_mm_loadu_si128`, `_mm_stream_si128`, `_mm512_i64gather_epi64`, `vld1q_u8`) are `unsafe fn`s on
the pinned nightly, and value-only ones (`_mm_sfence`, `_mm512_cvtepu8_epi64`, `vcreate_u64`, ...)
are safe, which is how ops are counted here. One comment above a `let` covers every single-op block
in that statement's tuple or array (clippy walks up through expressions to the `let`).

> forwarded to concurrency: `blaze-cas` and `blaze-reclaim` are its while it runs.

> applied by concurrency (child `concurrency-safety`, report
> `/tmp/blaze-lanes/v2-concurrency-safety.md`): all 24, the optional `table.rs` comment too;
> comments, `# Safety` sections and block splits only, evaluation order kept. `Chunk::bytes` is
> now `pub(crate) unsafe fn` with a `# Safety` section and a `// SAFETY:` at each caller
> (`Look::record`, `Store::drain_record`). In `arm::stnp` the pointers are bound as
> `(from_at, to_at)`, since `d` is an asm operand name there. `_mm_sfence()` in `x86::sse2` is
> outside `unsafe` (SSE is baseline on x86_64); a sweep found no other block in either crate
> that breaks the three lints.

### blaze-cas

- `crates/blaze-cas/src/chunk.rs:94` unsafe fn `cut_avx512`: no `# Safety`. Proposed, after its doc: `///\n/// # Safety\n///\n/// The CPU has AVX-512F: `cut` and `paths` call it only after `is_x86_feature_detected!("avx512f")`.`
- `crates/blaze-cas/src/chunk.rs:140` unsafe fn `cut_neon`: no `# Safety`. Proposed: `///\n/// # Safety\n///\n/// The CPU has NEON: `cut` and `paths` call it only after `is_aarch64_feature_detected!("neon")`.`
- `crates/blaze-cas/src/index.rs:282` unsafe fn `lanes_sse2`: no `# Safety`. Proposed: `/// # Safety\n///\n/// The CPU has SSE2: `sse2` hands it out only after detecting it.`
- `crates/blaze-cas/src/index.rs:302` unsafe fn `lanes_neon`: no `# Safety`. Proposed, after its doc: `///\n/// # Safety\n///\n/// The CPU has NEON: `neon` hands it out only after detecting it.`
- `crates/blaze-cas/src/cold.rs:69` `unsafe impl Sync for Prepared`: no SAFETY of its own (the comment above line 68 covers only `Send`). Proposed, between the two impls: `// SAFETY: as for `Send`: `&Prepared` reaches only `bytes` (a `Vec<u8>`) and the two digested dictionaries, which zstd only reads once made (zstd.h: a `ZSTD_CDict` and a `ZSTD_DDict` can be shared by many threads at once); `Prepared` has no method that changes them, so threads sharing it never race inside zstd.`
- `crates/blaze-cas/src/segment.rs:66` `unsafe impl Sync for Chunk`: no SAFETY of its own. Proposed, between the two impls: `// SAFETY: as for `Send`: through `&Chunk` other threads read only bytes a published place names or that were published, never written again, and the atomics; the writer writes only through `unsafe fn write` and `zero`, whose callers write past what was kept.`
- `crates/blaze-cas/src/segment.rs:144` `unsafe impl Sync for ChunkRef`: no SAFETY of its own. Proposed: `// SAFETY: a `ChunkRef` is an address: sharing it shares nothing until `ChunkRef::get`, whose caller promises the chunk is alive, and `Chunk` is `Sync`.`
- `crates/blaze-cas/src/segment.rs:93`, `:103`, `:147` unsafe fns `Chunk::write`, `Chunk::zero`, `ChunkRef::get`: their contract is a `/// SAFETY:` line, not a `# Safety` section. Proposed: replace each `/// SAFETY: <text>` by `///\n/// # Safety\n///\n/// <text>` (for `zero`, give it a first line: `/// Writes `len` zeros at `at`.`).
- `crates/blaze-cas/src/segment.rs:89` block: 2 ops (`add`, `slice::from_raw_parts`). Split:
  `// SAFETY: `at + len <= cap` (checked above), so `at` is inside the allocation of `cap` bytes.` then `let start = unsafe { self.bytes.as_ptr().add(at as usize) };` then
  `// SAFETY: the `len` bytes from `start` are inside the allocation, which lives as long as the chunk; bytes a place names or that were published are not written again.` then `Some(unsafe { std::slice::from_raw_parts(start, len as usize) })`.
- `crates/blaze-cas/src/segment.rs:100` block: 2 ops (`add`, `copy_nonoverlapping`). Split, keeping `data.as_ptr()` first:
  `let from = data.as_ptr();` then `// SAFETY: `at + data.len() <= cap` (asserted above), so `at` is inside the allocation.` `let to = unsafe { self.bytes.as_ptr().add(at) };` then
  `// SAFETY: `to` has `data.len()` writable bytes in the allocation, which `data` (the caller's own buffer) does not overlap, and no one else reads or writes them (the caller's contract).` `unsafe { ptr::copy_nonoverlapping(from, to, data.len()) }`.
- `crates/blaze-cas/src/segment.rs:107` block: 2 ops (`add`, `write_bytes`). Split:
  `// SAFETY: `at + len <= cap` (asserted above), so `at` is inside the allocation.` `let to = unsafe { self.bytes.as_ptr().add(at) };` then
  `// SAFETY: `to` has `len` writable bytes in the allocation, and no one else reads or writes them (the caller's contract, as in `write`).` `unsafe { ptr::write_bytes(to, 0, len) }`.
- `crates/blaze-cas/src/segment.rs:418` block: 4 ops (three `tail.write`, one `tail.zero`). Split into four statements, each `unsafe { ... };` with its own comment:
  `// SAFETY: this is the segment's one writer (`&mut self`), and `at` is past what it has kept, so no place points into `at..at + span` and nothing there was published; `roll` left room for `span` bytes.` over `unsafe { tail.write(at, &frame) };`,
  `// SAFETY: as above, for the header after the frame.` over `unsafe { tail.write(body, &header.encode()) };`,
  `// SAFETY: as above, for the payload after the header.` over `unsafe { tail.write(body + RECORD_HEADER as usize, payload) };`,
  `// SAFETY: as above, for the zeros up to `at + span`.` over `unsafe { tail.zero(end, at + span as usize - end) };`.
- `crates/blaze-cas/src/read.rs:119` block (`Drop for Shared`): 3 ops (three `Box::from_raw`). Split:
  `// SAFETY: `view` is a leaked box that only this drop frees; no reader is left, since each holds `self`.` `drop(unsafe { Box::from_raw(*self.view.get_mut()) });`,
  `// SAFETY: as above, for `owned`.` `drop(unsafe { Box::from_raw(*self.owned.get_mut()) });`,
  `let orphans = *self.orphans.get_mut();` `if !orphans.is_null() {` `// SAFETY: a leaked box (not null, checked) that `orphan` set once and only this drop frees.` `drop(unsafe { Box::from_raw(orphans) });` `}`.
- `crates/blaze-cas/src/copy.rs:42` (`avx512`), `:54` (`sse2`), `:66` (`stnp`) blocks: 3 ops each (`add`, the copy, `set_len`). Split each (shown for `avx512`; `sse2` and `stnp` alike without the CPU clause):
  `// SAFETY: `dst.len() <= dst.capacity()`, so the offset is inside (or one past) `dst`'s allocation.` `let end = unsafe { dst.as_mut_ptr().add(dst.len()) };`
  `// SAFETY: the CPU has AVX-512F, asserted above; `reserve` left room for `src.len()` bytes at `end`, and `src` is borrowed shared while `dst` is borrowed mutably, so they don't overlap.` `unsafe { x86::avx512(end, src) };`
  `// SAFETY: `x86::avx512` wrote all `src.len()` bytes after the old length, and `reserve` made the capacity at least the new length.` `unsafe { dst.set_len(dst.len() + src.len()) };`
- `crates/blaze-cas/src/copy.rs:94` block (`x86::avx512`): about 9 ops (`copy_nonoverlapping` twice, `add` four times, `_mm512_loadu_si512`, `_mm512_stream_si512`; `_mm_sfence` is safe). Split:
  `// SAFETY: `head <= from.len()` (`split`); `src` is valid for reads and `to` for writes of `from.len()` bytes, and they don't overlap (the caller's contract).` `unsafe { ptr::copy_nonoverlapping(src, to, head) };`
  `let mut at = head;` `while at < head + body {`
  `// SAFETY: `at + 64 <= head + body <= from.len()`, so `at` is inside `from`.` `let from_at = unsafe { src.add(at) };`
  `// SAFETY: the 64 bytes at `from_at` are inside `from`; the load takes any alignment.` `let v = unsafe { _mm512_loadu_si512(from_at.cast()) };`
  `// SAFETY: `at < from.len()`, for which `to` is valid (the caller's contract).` `let to_at = unsafe { to.add(at) };`
  `// SAFETY: `to_at` is 64-byte aligned (`split` aligned `to + head`, and `at` moves by 64) and has 64 writable bytes (`at + 64 <= from.len()`).` `unsafe { _mm512_stream_si512(to_at.cast(), v) };`
  `at += 64;` `}` `_mm_sfence();`
  `// SAFETY: `at <= from.len()`, so both offsets are inside (or one past) their buffers.` `let (from_at, to_at) = (unsafe { src.add(at) }, unsafe { to.add(at) });`
  `// SAFETY: the `from.len() - at` bytes from each are in `from` and `to` (the caller's contract), which don't overlap; the non-temporal stores were fenced above.` `unsafe { ptr::copy_nonoverlapping(from_at, to_at, from.len() - at) };`
- `crates/blaze-cas/src/copy.rs:119` block (`x86::sse2`): the same split with 16 for 64, `_mm_loadu_si128` and `_mm_stream_si128` (`_mm_sfence` is safe where SSE is baseline).
- `crates/blaze-cas/src/copy.rs:153` block (`arm::stnp`): `copy_nonoverlapping` twice, `asm!`, `add` four times. Split as for `x86::avx512`; in the loop bind `// SAFETY: `at + 64 <= head + body <= from.len()`, so `at` is inside both buffers.` `let (s, d) = (unsafe { src.add(at) }, unsafe { to.add(at) });` (the operands are evaluated in this order today) then `// SAFETY: NEON is baseline on aarch64; the asm reads the 64 bytes at `s`, in `from`, and writes the 64 at `d`, in `to` (the caller's contract), which don't overlap.` `unsafe { asm!(..., src = in(reg) s, dst = in(reg) d, ...) };`.
- Optional, comment that says less than it could: `crates/blaze-cas/src/table.rs:193` says why `old` is owned but not why it is a box. Proposed: `// SAFETY: `old` is not null: `probe` found a value in this slot, and a slot once set is only swapped for another `Box::into_raw`, never cleared. The swap unlinked it, and the one caller that swaps (the store, under `&mut`) owns it now; readers that found it keep reading it until the limbo frees it.`
- `crates/blaze-cas/src/segment.rs:82` MAY BE UNSOUND (API, not a block): `Chunk::bytes` is a safe `pub(crate) fn` whose soundness rests on "callers read only what a place names or what was published": a safe caller asking for any other in-bounds range reads bytes the writer may be writing with `copy_nonoverlapping` (a data race). Every caller today keeps the rule (`Look::record` through a spot's `Seg`, the store's drain through `published()`). Proposed: make it `unsafe fn bytes` with `# Safety: the range is one a published place names, or below what the chunk published`, and a SAFETY comment at each caller; or check against `published()` where that covers the caller.
- Checked, no finding: every zero-copy view of mapped pages. The crate has no `unsafe` on mapped bytes. Packs and files of their own are read as safe `&[u8]` slices of `blaze_core::Mapping`, bounds-checked by `Pack::bytes` (`pack_bytes` gives `None` outside the mapping) and `Chunk::bytes`. `Look::value` hashes each value's bytes against `header.content` before it hands out a `Cow::Borrowed` view (`Form::Raw`, `Form::External`), and `get_in_place` goes through `value` before returning a warm payload. `Look::kept` reads only the header of an unverified record, to dedupe a put.
- Note (no change asked): `table.rs:193`, `read.rs:77` and `:85` turn an unlinked pointer into a `Box` while other threads may still hold `&T` into it, and then move the box into the limbo. That is sound under Tree Borrows, which our Miri runs (docs/v2/plan/design/unsafe.md), because nothing writes or frees through the box until the grace has passed. Stacked Borrows would report it. If `blaze test --ub` ever runs Stacked Borrows, make `Limbo` hold the raw pointer and a drop fn, and call `Box::from_raw` only at the free.

### blaze-reclaim

- `crates/blaze-reclaim/src/lib.rs:95` block `unsafe { &*other }`: no SAFETY of its own (the comment above line 94 is the `drop` statement's). Proposed, above it: `// SAFETY: `other` is the block another thread linked on: leaked with `Box::into_raw` and never freed, so it lives for the process.` and narrow line 93 to `// SAFETY: `new` was never published, so this is its only owner.`
- Checked, no other finding. Each of the other six blocks does one op and names what keeps its memory alive: a linked block is leaked and never freed (`:66`, `:91`), and a test node is freed only after a grace that its reader's task is inside (`tests.rs:140`, `:158`, `:171`).

## Status of what was forwarded here (concurrency, 2026-10-29)

- C6: done in `lib.rs` (`put_hashed`, `take_fetched` parses the frame in place and hashes once)
  and `segment.rs` (`Own::put` takes the content and `&[u8]`). A fetch's `Done::Read` now
  carries the whole answer frame (8-byte length, then the answer).
- C1-C5, C7, C8: child `concurrency-remote` (owns `remote.rs`); report
  `/tmp/blaze-lanes/v2-concurrency-remote.md`.
- safety-audit findings for `blaze-cas` and `blaze-reclaim`: child `concurrency-safety`;
  report `/tmp/blaze-lanes/v2-concurrency-safety.md`.
- core-linux item 6 (packs written as block-aligned `WriteAt`): done; `Op::Append` is now
  `Op::WritePack`, a `Truncate` then a `WriteAt` at the committed end.
- usercode-concurrency item 3 (`blaze_cas::Reader`): exists, from `Store::reader()`.
- Root `Cargo.toml` `blaze-reclaim = { path = "crates/blaze-reclaim" }`: already there.

> applied: noted; a status report, nothing asked.

## Requests from concurrency (2026-10-29)

### R1. format: the lock ban in `crates/blaze-ids/` and `crates/intern/`

clippy reads only the nearest `clippy.toml`, so the hot-crate ban needs a copy per crate. Copy
`crates/blaze-cas/clippy.toml` to `crates/blaze-ids/clippy.toml` and `crates/intern/clippy.toml`,
landing with the fixes for the audit findings in `v2/requests/format.md` (until then the copy
fails clippy on the locks those findings name).

> forwarded to format: `crates/blaze-ids/` and `crates/intern/` are its.

### R2. core: the lock ban in `crates/blaze-graph/`, and each job a reclamation boundary

- Copy `crates/blaze-cas/clippy.toml` to `crates/blaze-graph/clippy.toml`, with the fixes for the
  findings in `v2/requests/core.md`.
- Take `let _task = blaze_reclaim::enter();` around each job `crates/blaze-graph/src/pool.rs`
  `Local::run` (~line 778) runs, and add `blaze-reclaim.workspace = true` to blaze-graph's
  `Cargo.toml`. Then every plane task is a quiescent boundary, and a store read inside a job can
  take `blaze_reclaim::enter()` for free (guards nest). Without it, values the store retires are
  freed only when the threads that read them next leave a guard they took themselves.

> applied (core DONE): `crates/blaze-graph/clippy.toml` is a copy of blaze-cas's; `Cap`'s state is pump's `Lock` (the one allowed lock) and `Lock` names `std::sync::Mutex` in place, so no import trips the ban; each test module says `#[allow(clippy::disallowed_types, reason = ..)]` as `exec.rs`' did. `Local::run` (`pool.rs`) takes `let _task = blaze_reclaim::enter();` around each job, and blaze-graph depends on `blaze-reclaim.workspace = true` (a new edge for the lock).

### R3. fastcompile-global: frame the answers `blaze-global`'s `Remote` hands the store

`crates/blaze-global/src/client.rs:663` and `:684` build `Done::Read(Answer::...encode())`.
The store now reads a fetch's `Done::Read` as the whole frame (8-byte LE length, then the
answer), so a remote can hand on the receive buffer without a copy (C5). Once
`concurrency-remote` lands `Answer::encode_framed`, use it at both sites. Until then those
answers are refused as malformed (an error, never a wrong value).

> forwarded to fastcompile: `crates/blaze-global/` is fastcompile-global's.
> applied (fastcompile-global): both answers in `client.rs` (`fetched`, the missing and the kept value) are `Answer::encode_framed()`. A refused value stays `Done::Failed`.

### R4. store (or the coordinator): `docs/v2-store.md`

It still describes `put` appending to memory under one writer, `Append`, and flags indexed by
row with a delta entry of 48 bytes. Describe instead: `Reader` and `Writer`, per-worker
segments, spots retargeted to packs after mapping, `WriteAt` pack writes, and point at
`docs/v2/plan/lanes/concurrency/store.md`.

> forwarded to coordinator: `docs/v2-store.md` is not in any lane's paths in crates.md, so it falls to the coordinator.

## From safety-fuzz-cas (2026-10-29)

Fuzz targets for blaze-cas: `fuzz/targets/cas_{pack,head,index,entry,cold,remote}.rs`, with
every Store/remote call in `fuzz/targets/cas_shared.rs` (a module, not a binary). The calls
are `Store::{open, complete, owns, ready, broken, get, verify, put, flush, settle, fetch,
fetched, reader}`, `Reader::get` with `blaze_cas::enter()`, and `remote::{Answer::{of, encode,
decode}, Client::{new, complete, owns}, Server::{new, start, complete, owns}, LONGEST_VALUE}`.
If you change those, adapt `cas_shared.rs` (or ask safety-fuzz-cas). Each input below is a
corpus file, kept, and fails its target today (found by reading; nothing was run). The
targets' first byte is a mode: `02` is "these pack bytes, every record header's CRC computed
by the target" (the `00000000` in the hex are placeholders for it).

> forwarded to concurrency: `crates/blaze-cas/` is its.

### F1. A record's untrusted `raw` is allocated before the frame is read (cold.rs 126-163)

`decompress`, `decompress_long` (zstd's bulk `decompress(stored, raw)` does
`Vec::with_capacity(raw)`) and `unlz4` (lz4_flex `decompress(stored, raw)` does `vec![0; raw]`)
take `raw` from the record header. A 16-byte payload with `raw = 2^40` asks for 1 TiB: abort,
or a huge allocation for any `raw` up to what the machine has. `fuzz/corpus/cas_pack/zstd-raw-huge.hex`:

    02 424c5a5041434b04 0000000000000000
    424c5a52 02 01 0000 ffffffff 00000000 1000000000000000 0000000000010000
    11×32 22×32 28b52ffd00000000 0000000000000000

(`lz4-raw-huge.hex` is the same with kind 04, form 05.) Fix: bound `raw` by what the bytes can
decode to before allocating: for zstd read the frame's content size
(`zstd_safe::get_frame_content_size`, refuse unknown or `!= raw`) or decompress streaming into
a buffer grown as output comes; for LZ4, refuse `raw > 255 * stored + 16` (a block's densest).

### F2. Chunk lengths sum unchecked; `with_capacity(raw)` (read.rs 589-594)

`chunks.iter().map(..).sum()` of untrusted u64s overflows (a panic in debug, a wrap in release
that can then equal `raw`), and `Vec::with_capacity(total)` with `total == raw` (2^40) runs
before any chunk is found. `fuzz/corpus/cas_pack/chunked-lengths-overflow.hex` (lens
`u64::MAX` and 3, raw 2) and `chunked-raw-huge.hex` (one chunk of 2^40, raw 2^40). Fix:
`checked_add` the lengths (overflow is `Corrupt`), and grow `whole` as pieces arrive (or cap the
reservation at the sum of the pieces' `raw` once found).

### F3. `indexed` is not checked to be a record boundary (lib.rs 2350-2352)

Head decode checks `indexed <= committed`, not that it is 16-aligned. `indexed = committed - 1`
makes `all[at..at + 4]` slice past `all` and panic in open. `fuzz/corpus/cas_head/indexed-not-a-record.hex`
(packs 1, committed 4096, indexed 4095, the lone pack beside it):

    02 424c5a4845414404 03000000 00000000 0100000000000000
    ff00000000000000 0000000000000000 00000000 00000000 0000000000000000
    01000000 00000000 00000000 00000000
    00 000000 00000000 0010000000000000 ff0f000000000000 0000000000000000 00000000

Fix: head decode refuses an `indexed` (and `dead`) that is not a multiple of 16, and the scan
uses `all.get(at..at + 4)` so a short tail is the "neither a record nor zeros" error.

### F4. `RECORD_HEADER + stored` overflows: a panic in debug, a hang in release (disk.rs 217)

`span()` is `aligned(RECORD_HEADER + self.stored)`. `stored = 2^64 - 96` overflows: debug
panics; release wraps to span 0, `end == at`, and `open_tier`'s loop (lib.rs 2353-2376) spins
forever. Same pattern: read.rs 543 (`RECORD_HEADER + seg.stored`), lib.rs 1066 and 1179.
`fuzz/corpus/cas_pack/stored-overflows-span.hex`:

    02 424c5a5041434b04 0000000000000000
    424c5a52 00 00 0000 00000000 00000000 a0ffffffffffffff 0000000000000000 11×32 22×32

Fix: `RecordHeader::decode` refuses `stored > PACK_MOST` (no record is longer than a pack),
and `span()` uses `checked_add` or is only ever called on decoded headers.

### F5. A client reserves a frame's length before its bytes come (remote.rs 477)

`up.buffer.reserve(owed(&up.buffer))` reserves up to `LONGEST_FRAME` (1 GiB) on an 8-byte
prefix: any server (or anything on the connection) makes the client allocate 1 GiB per
connection. `fuzz/corpus/cas_remote/frame-of-a-gigabyte.hex`: `00 00 0000 0000004000000000`
(one fetch, then the 8 bytes `len = 2^30`). Fix: reserve at most what the next receive can
bring (`CLIENT_RECV`) and let the buffer grow with the bytes; `pump`'s `most` likewise need not
be the frame's whole length.

### Note: the client and the store disagree on framing (not a new bug; R3)

`Client::complete` hands `Done::Read(body)` (remote.rs 459/472, as its doc says), while
`take_fetched` (lib.rs 2280) now expects the frame with its length. `cas_remote` checks each
fetch through a real `Client` into a `Store` ends as its frame says, so its seeds fail until
`concurrency-remote` makes the client hand on the frame. Nothing in `cas_remote` needs changing
when it does.

> applied by concurrency (2026-10-29), nothing built:
> - F1 `cold.rs`: `zstd_length` refuses a frame whose declared content size is missing or is not
>   `raw`, and a `raw` above what its bytes can decode to (3-byte block headers, 128 KiB per
>   block), before allocating; `unlz4` refuses `raw > 255 * stored + 16`. Test
>   `a_length_the_bytes_cannot_reach_is_refused_before_it_is_allocated`.
> - F2 `read.rs`: a chunk length over `chunk::MAX` is `Corrupt`, lengths are `checked_add`ed,
>   and the value grows as its pieces are read (at most 64 MiB reserved, once the first is).
> - F3 `disk.rs`: head decode refuses an `indexed` or `dead` that is not a multiple of 16;
>   `open_tier` refuses a pack mapped shorter than `committed` (a second panic in the same
>   lines) and reads a record's first word with `get`. Head test extended.
> - F4 `disk.rs`: `span_of(stored) -> Option<u64>`; `RecordHeader::decode` refuses a `stored`
>   with no span, so `span()` holds for every header. lib.rs: `movable` refuses a record whose
>   header is not the `stored`/`raw` its index slot names, so `replace`'s dead count uses a
>   checked length; settle's budgets add `loc.raw` saturating; the sample read's offset is
>   checked. read.rs 543's `seg.stored` comes from the writer's own put and is not untrusted.
>   Test `a_record_header_of_a_length_no_record_has_is_refused`.
> - F5 `remote.rs`: the client reserves at most `CLIENT_RECV_MOST` (one receive) on a prefix.
>   Test in `a_long_answer_is_received_in_reads_of_what_it_still_needs`.
> - The framing note is settled: the client hands on the whole frame (`concurrency-remote`).
>   But C8 changed `Client::complete` to `(completion, effects, answers: &mut Vec<Completion>)`
>   returning nothing (and `close(effects, answers)`): `fuzz/targets/cas_remote.rs:212` must
>   adapt. Asked of safety-fuzz-cas.
