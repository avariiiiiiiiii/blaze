---
status: plan
summary: blaze v2 restore on Linux: the batched IORING_OP_STATX sweep of tracked paths on blaze-core's ring, and the journal effect on fanotify or inotify.
date: 2026-10-29
---
# Changed files on Linux: a statx sweep on the ring, then a live watch

Part of [restore](../../../design/restore.md) ("Changed files come from the OS's journal").
The code is `crates/blaze-core/src/uring.rs`; the types are in `crates/blaze-core/src/journal.rs`.

## Linux

Linux keeps no journal of changes across a restart. A warm restart therefore stamps every tracked
path once and compares each stamp (inode, size, mtime, ctime, mode) with the column saved in the
image. Only paths whose stamp moved are hashed, and the directories whose stamp moved are listed.
Live changes come from the watch from then on.

### `Effect::Stamp` on the ring

- **One `IORING_OP_STATX` per name.** The request's `dirfd` is the root, opened once per sweep and
  kept until the sweep's last completion, because the kernel resolves a name against the
  descriptor's number when it runs the request. Its flags are `AT_SYMLINK_NOFOLLOW |
  AT_STATX_DONT_SYNC`, and its mask is `STATX_TYPE | MODE | INO | SIZE | MTIME | CTIME`. The name
  is `Names`' bytes in place, already NUL-terminated, so nothing is allocated or copied per name.
- **As many in flight as the submission ring holds**, which is 4096. One pool of that many `statx`
  buffers (256 bytes each, 1 MiB, mapped once for the driver's first sweep) is lent one buffer per
  request and returned as each completion is reaped. Memory stays at 1 MiB however many names
  there are. A buffer per name would be 25 MiB for 100k names, and that is what the brief's "one
  `Vec<statx>` per batch" would have cost.
- **Batched submission.** `Ring::push_each` writes as many requests as fit and publishes them with
  one release store of the tail, so a fill costs at most one `io_uring_enter`, and with SQPOLL
  usually none. Every reap refills the ring. Requests waiting for room in the ring go before
  sweeps, so a sweep never starves a read or a write.
- **`user_data`** packs a stamp bit (bit 63), the sweep's slot (15 bits), the buffer (16 bits)
  and the name's index (32 bits). Every other request's `user_data` is `id << 2 | link` with an id
  below 2^61, or `NOBODY`. Any number of sweeps can be in flight with each other and with every
  other effect. They are fed in order, and the result vector of each is written by index, so the
  stamps come out in the names' order.
- **Stamps** are `crate::fs::stamp_of`'s: `sec * 1e9 + nsec` wrapping, with `mode` taken from
  `stx_mode`. A name with nothing there (`ENOENT`, `ENOTDIR`) is `Stamp::GONE`. Any other error,
  or a `statx` that did not fill a field asked for, is `Stamp::UNKNOWN`, so the asker looks at
  that name and meets the error itself. The effect as a whole fails only when the root cannot be
  opened.
- **Failure and drop.** A ring that fails ends every sweep with `Failed`. Its names, directories
  and buffers are never freed, and no later sweep starts on that ring. Dropping the driver waits
  for the kernel to give back every sweep request, and leaks what it does not give back in time.

### `Effect::Entries` and `Effect::Journal`

- `Entries` runs `crate::fs::entries` on a thread off the ring, as `List` does, because io_uring
  has no `getdents`.
- `Journal` is a watch (`crate::watch::Tree`: fanotify, or inotify without the privilege). Its
  first completion is `journal::first_on_linux`, which says the changes since any saved cursor
  are `Lost::NoJournal`. Each batch the tree reads after that, including those drained by a flush,
  goes through `journal::live`. `Done::Journaled` does not end the effect, and `Unwatch` ends a
  journal as it ends a watch.

### The benchmark

- **Target:** the restore doc's budget of about 50 ms for 100k tracked files on a warm restart,
  that is, with the dentry and inode caches holding the tree, as after a daemon restart without a
  reboot.
- **State of the art:**
  - Watchman's initial crawl does a `readdir` plus `lstat` per entry on a thread pool, and pays
    for it on every start that has no saved clock. Its warm query is fast only because the process
    stayed up.
  - `fd` and ripgrep (the `ignore` crate's parallel walker) avoid `stat` wherever `d_type` is
    enough, and walk a hot tree of 100k entries in tens of milliseconds over all cores.
  - A hot-cache `lstat` costs roughly 0.3 to 1 µs of kernel time per name, so 100k names are 30 to
    100 ms on one core.
  - io_uring always runs `STATX` on its io-wq workers (`REQ_F_FORCE_ASYNC`), up to
    `min(SQ entries, 4 × CPUs)` of them in parallel for bound work. 4096 in flight therefore spread
    the sweep over every core with no thread of ours, and leave the driver's thread free.
- **What beats it:** no walk at all. The tracked paths come from the image, so no `getdents` is
  needed, and no directory is opened but the root. Directories are listed only where their own
  stamp moved. One ring holds every request, and nothing is allocated per name.
- **Measured by:** `uring::tests::a_sweep_of_100k_names_is_within_the_restart_budget`. It is
  ignored by default because it makes 100k files. It times the best of five warm sweeps against
  50 ms and prints the threaded reference's time (`fs::stamps`, `fstatat` on 8 threads) beside it.
  No number is recorded here until the coordinator runs it.
- **Cold cache** (after a reboot): each `statx` may read an inode from disk. The 4096 requests in
  flight keep an NVMe queue deep, but this is outside the 50 ms budget, which is for restarts.

### What the tests prove

- `a_sweep_stamps_as_the_reference_does`: 10 000 files (more than the submission ring holds),
  plus directories, a symlink, a dangling symlink, an executable file, and names with nothing
  there (one under a file, for `ENOTDIR`). Name by name, the ring's `Done::Stamped` equals
  `crate::fs::stamps`, the safe reference. Every buffer and slot is returned afterwards.
- `sweeps_in_flight_together_and_with_other_effects`: two sweeps at once, one of them in reverse
  order, plus an empty sweep, an append, a timer and two `Entries` (one of a directory that is not
  there). Each ends as the reference has it. A sweep of a missing root fails, naming the root.
- `a_journal_tells_what_is_lost_then_what_is_written`: with a saved cursor, the first batch says
  `NoJournal` for the directory, and with `Cursor::None` it says nothing is lost. A file written
  afterwards is told as a live `Journaled` before a flush asked after it, and `Unwatch` ends the
  journal with nothing left in flight.
