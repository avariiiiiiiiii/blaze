# Requests to `concurrency` (owner of `crates/blaze-cas/`)

## From speed-link-edge-remote: the remote tier (2026-10-29)

Paths: `crates/blaze-cas/src/remote.rs`, and `Store::take_fetched`/`put` in
`crates/blaze-cas/src/lib.rs`. Before and after numbers:
`docs/v2/plan/lanes/store/speed-link-edge-remote.md`. Nothing outside `blaze-cas` calls
`blaze_cas::remote` yet, so the signature changes below break no caller. C1 is a memory bound,
so do it even if you skip the rest.

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

### C3. Client: one send per turn of fetches (`impl Remote for Client`, ~line 503)

- Now: `fetch` pumps at once, so the first ask of a turn is sent alone. The asks of the turn's
  other fetches wait for that `Done::Sent`: a second send, and up to one more round trip.
- Change: in `fetch`, when `Link::Up(up)` has `up.sending == Some(tag)`, look for that send still
  unsubmitted, `effects.iter_mut().rev().find_map(|e| match e { Effect::Send { tag: t, bytes, .. } if *t == tag => Some(bytes), _ => None })`.
  If it is found, `write_ask(bytes, key)` into it. Otherwise write to `self.out` as now.
  `waiting.push_back` as now.
- Test: 24 fetches with one `effects` `Vec` give exactly one `Effect::Send` of 24 × 40 bytes.

### C4. Client: adopt the driver's receive `Vec` (`Client::complete`, `Role::Recv`, ~line 381)

- Change: `Done::Read(bytes)` →
  `if up.buffer.is_empty() { up.buffer = bytes } else { up.buffer.extend_from_slice(&bytes) }`.
- Saves a copy of every answer that arrives in one receive, and of the first receive of a long
  one. The existing split-at-N tests cover it.

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

### C8. `Client::complete` and `close` append to a caller's `Vec`

- Change: `complete(&mut self, completion, effects, answers: &mut Vec<Completion>)`, and the
  same for `close`. This drops an allocation per completion that answers something. The
  callers are the tests only.

### Still needs core

`Effect::SendMapped` and `Effect::RecvInto` are requested in `v2/requests/core.md` (K1, K2).
- With K1, the server sends a `Form::Raw` record's bytes from the pack mapping: zero copies in
  user space.
- With K2, a long value lands with one copy across receives.
