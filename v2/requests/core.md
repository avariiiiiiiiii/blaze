# Requests from lane core

## 1. Workspace dependency `crossbeam-deque`

- What: add `crossbeam-deque = "0.8.6"` to the root `Cargo.toml`'s `[workspace.dependencies]`
  (already in `Cargo.lock` through rayon, so the lock does not change).
- Where: root `Cargo.toml`; used by `crates/blaze-graph/Cargo.toml` (`crossbeam-deque.workspace
  = true`).
- Why: the worker pool (docs/v2/plan/lanes/core.md section 4) steals work between threads; each worker's
  deque is crossbeam's Chase-Lev deque, so the hot path takes no lock.

---
Applied by the coordinator, 16:46.

> applied: `crossbeam-deque = "0.8.6"` is in the root `[workspace.dependencies]` (by the coordinator, 16:46).

## 2. usercode: a `blaze_core::Confine` over the sandbox

- What: implement `blaze_core::Confine` (`crates/blaze-core/src/lib.rs`) over the sandbox in
  `crates/blaze-cargo/src/sandbox*` (Landlock and seccomp on Linux, Seatbelt on macOS):
  `confine(command, scratch)` returns the program, arguments, environment and directory that run
  `command` under `Sandbox::On { read, write, network, trace }`, with its trace written under
  `scratch`; `traced(command, scratch)` reads that trace back as a `Traced`. Hand it to the
  drivers with `Threads::confined`, `Uring::confined` and `Kqueue::confined`.
- Where: `crates/blaze-usercode/` (usercode), and whoever builds the daemon's driver.
- Why: core does not own the sandbox. A driver without a `Confine` fails every
  `Sandbox::On` spawn with an error naming it, so build scripts and proc macros cannot run
  under `Effect::Spawn` until this exists (sandbox-everything: nothing runs outside by default).

> applied: by usercode (`impl Confine for Launcher` in `crates/blaze-usercode/src/confine.rs`).
> forwarded to cargo: handing it to the daemon's driver (`Threads::confined`, `Uring::confined`, `Kqueue::confined`) is the daemon's.

## 3. daemon: count allocations and run work on the scheduler

- What: in the daemon binary, `#[global_allocator] static ALLOC:
  blaze_graph::arena::Counting<std::alloc::System> = blaze_graph::arena::Counting::new(std::alloc::System);`,
  and the daemon's CPU work (queries, codegen, links) posted through
  `blaze_graph::exec::Scheduler` as `Task`s with their `TaskKind`, the plane's budget from
  `blaze_graph::arena::machine_budget`.
- Where: the crate holding the daemon's `main` (cargo or ci), `crates/blaze-cargo/src/memory.rs`.
- Why: memory admission (docs/v2/plan/lanes/core.md section 4) learns each kind's peak from what the
  counting allocator measures per task and holds the daemon's own arenas against the budget;
  without the allocator every measured peak is 0 and admission admits everything.

> forwarded to cargo: the daemon's `main` still uses `tikv_jemallocator::Jemalloc` (`crates/blaze-engine/src/lib.rs`) and `crates/blaze-cargo/src/` uses no `blaze_graph::exec::Scheduler`; lane cargo's daemon owns it (wrap its allocator in `blaze_graph::arena::Counting`).

## 4. Applied: link's request 3 and vfs's request 1

`Effect::WriteAt`, `Effect::Rename`, `Effect::Mode` (`Done::Written`, `Done::Renamed`,
`Done::Moded`) and `Effect::List` (`Done::Listed`) are in `crates/blaze-core/src/lib.rs`, in
the simulation, `Threads`, `Uring` and `Kqueue`. `Sim::mode(file)` reads a file's mode.

> applied: note only.

## 5. Note for vfs, store and db: the simulation's truncate is now undone by a crash

The simulation's files keep a log of unsynced changes (appends, writes at an offset,
truncations); a crash keeps a random prefix of it, the last possibly torn. A truncate not yet
covered by a completed `Sync` can now be lost by a crash, as on a disk (before, the cut always
survived). A journal that cuts a torn tail and appends after it must not count the cut durable
until a sync after it completes; tests that relied on the old rule may now fail on some seeds.

> applied: note only, for vfs, store and db.

## From concurrency: lock audit (2026-10-29)

Audited against `docs/v2/plan/design/concurrency.md`. The plane (`plane.rs`, `machine.rs`,
`admission.rs`) takes no lock. The pool takes none per task: `pool.rs:76` `stray` is locked
only when a job's `done` panics. The per-query locks are all in `pump.rs`, and they are on
every memo miss: `blaze-query`'s `single` (`crates/blaze-query/src/lib.rs:1139`) calls
`Flights::get_or_run` for each key a view's `results` does not have yet, including each green
check on a warm build.

1. **`crates/blaze-graph/src/pump.rs:105` `WAITS`, one process-wide `Mutex<Waits>` (a
   `BTreeMap` of wait edges).** It is taken at `:309` in `Ending::end` on *every* run's end,
   even a run with no waiters: the loop over `waiters` is empty, but the lock comes first. That
   contradicts the module doc (`:12-13`, "nothing of a run that waits for nobody takes it").
   It is also taken at `:60` and `:71` around every pumped job, and at `:267` for every wait.
   This breaks two rules: a shared write per query ("Writers shard by hash"), and a single
   global lock on the hot path. Every core serializes on one cache line at each query end.
   Fix, first step: in `end`, take `WAITS` only when `waiters` is non-empty.
   Fix, the real one: store edges per task, not in a global map. Give each task's `on` and
   `above` a home in an atomic word of a slot table indexed by task id (per-worker slots,
   padded to 128 bytes). A waiter publishes its `on` edge with a SeqCst store, then walks the
   chain with acquire loads. If the walk reaches itself, it retracts the edge. When two waits
   close one cycle at once, only the task with the largest id in the cycle gives way, so
   exactly one gives way. Neither a wait nor a pump then takes a lock.
2. **`pump.rs:36`, `:202`, `:213`, `:250`: `Flights` has a fixed 64 shards, indexed by
   `key.0[0] % 64`.** Every `get_or_run` takes a shard lock twice, in `join` (`:254`) and
   `Ending::end` (`:300`). It also allocates an `Arc<Flight>` holding a `Mutex` (`:278-281`)
   for every run, on the global allocator. This breaks "shard count a power of two, at least
   4 times the core count, chosen at startup". The shard lock is held only to claim, never
   while running, which follows the rule.
   Fix: size the shards in `Flights::new` from the topology, as
   `(4 * cpus).next_power_of_two()`, and index by masked key bits. Better still, drop the
   shards: use an open-addressed claim table keyed by the key's hash. A claim is a CAS of the
   slot's state word from empty to running(task id). Waiters push onto a lock-free list in the
   slot. The value is published once (see item 3), and the slot is freed at the plane's task
   boundary. Then no `Arc` or `Mutex` is allocated per run.
3. **`pump.rs:160` `Flight::state: Mutex<State<V>>`, polled by waiters.** `outcome()` (`:166`)
   and `ended()` (`:174`) lock it on every spin of `wait` (`:331-351`, up to `SPINS = 64`
   spins, then on every park check). So readers write the lock word of the runner's flight,
   which breaks "A read writes no shared memory". Fix: keep the state in an `AtomicU8`
   (running, done or cancelled), store the value in a `OnceLock<V>` set before a release store
   of `done`, and have waiters acquire-load it. The waiter list then needs the lock-free list
   from item 2, or a short claim lock taken only to push a waiter and never by a poll.
4. **`crates/blaze-graph/src/arena.rs:30`, `:37`, `:68`: `HELD` is 64 fixed shards of
   `AtomicI64`, assigned round-robin.** `Counting` does a relaxed `fetch_add` on one of them
   for *every* allocation in the daemon. With more than 64 threads, threads share a line, and
   the count does not scale with the machine. This is an atomic, not a lock, but it breaks the
   fixed-64 rule on the hottest write path there is. Fix: keep the count in the thread's own
   slot, in a startup-sized array of 128-byte-padded counters, one per thread slot (the
   thread-local `NET` already holds it). Have `held()` sum the slots; it runs at admission and
   in `machine_budget`, not per allocation.
5. **`arena.rs:338` `Cap::state: Mutex<State>` plus `Condvar`.** Every `Bump` chunk grow under
   a cap (`:280`, `reserve` at `:427-475`) and every `Grant` drop (`:509-525`) takes one
   process-wide lock. Each drop also calls `notify_all`, which wakes every waiter. A full cap
   blocks the *worker thread* in `Condvar::wait` (`:474`), which breaks "No thread waits on
   another's work". Nothing outside `arena.rs` builds a `Cap` or a `Bump::under` yet, but this
   is the designed per-task arena path. Fix: keep `held` in an `AtomicU64` and reserve with a
   CAS loop while `held + bytes <= limit`, with no lock. Keep `by_holder` per worker slot.
   Only an over-limit reserve takes the slow path, and it should hand the task back to the
   plane to park (return "would wait") instead of blocking the thread.

   Status of 4: done by speed-runtime, another way. `held()` runs after every task's `Done`
   (`exec.rs`, `blaze-cargo/src/serve/plane.rs`), so summing one slot per thread there costs a
   cache line per thread per task. Instead, each thread's count reaches one padded total only
   once it drifts 256 KiB (Linux's `percpu_counter`), and is flushed at thread exit. An
   allocation does no atomic, and `held()` is one load, exact for the calling thread and within
   256 KiB for each other one.

   Status of 5: partly done by speed-runtime. `Bump::reset` now keeps its last chunk, so a
   per-task arena reset between tasks reserves nothing from the cap in steady state: the lock
   is taken once per new chunk (64 KiB or more), not per task. A `Grant` drop no longer wakes,
   which is a futex syscall, unless someone waits. The wait itself stays: core.md section 4
   says "a task that would pass the cap waits", and the cap cancels the newest waiter when
   every holder waits. A lock-free reserve would need per-thread holder counts for that check.
   Not done, because the lock is no longer on a per-task path.
6. **`crates/blaze-core/src/kqueue.rs:202`, `:907-909` (the macOS driver) and
   `crates/blaze-core/src/threads.rs:50`, `:155-157`: the file-effect queue is an
   `Arc<Mutex<mpsc::Receiver<Effect>>>`.** An idle IO thread holds the mutex *while blocked
   in `recv`*. So every file effect (`ReadAll`, `Read`, `Stat`, `Map`, ...) is handed off
   through one lock, and the other IO threads queue on the mutex rather than on the channel.
   This is a single global lock on every file read the macOS driver does. Fix: use
   `crossbeam_channel`'s MPMC `Receiver`, cloned into each thread, which is lock-free. Add
   `crossbeam-channel.workspace = true` to `crates/blaze-core/Cargo.toml` (already a
   workspace dependency, used by `blaze-graph`). Or give each thread its own queue and submit
   round-robin.
7. **For codegen's finding on `hir-ty-codegen/v2/frontend.rs` (in `codegen.md`): expose this
   thread's worker index.** `pool.rs` keeps `Local::index` but exports only `group()`. Add
   `pub fn worker() -> Option<u32>` beside `group()` (`pool.rs:98`) so per-worker tables can
   be indexed without a lock.

> forwarded to core: every finding is in lane core's own `crates/blaze-graph/`; lane core is still running.

## From speed-link-edge-remote: two socket effects for the remote tier (2026-10-29)

For `crates/blaze-cas/src/remote.rs` (the laptop/VM read-through tier). The before and after
numbers are in `docs/v2/plan/lanes/store/speed-link-edge-remote.md`, and the rest of that work
is in `v2/requests/concurrency.md`. These are the two copies left there that only
`blaze-core`'s effect API can remove.

K1. **`Effect::SendMapped { conn: Conn, mapping: Mapping, range: std::ops::Range<u64>, tag: Tag }`,
    completing `Done::Sent`.**
    - Sends `mapping.0.as_ref()[range]` on `conn` without copying it into a `Vec`:
      - Linux: io_uring `IORING_OP_SEND_ZC` over the mapped range, or `splice`/`sendfile`
        from the pack's file;
      - macOS: `sendfile(2)` (kqueue driver);
      - threads driver: `write` straight from the slice.
    - The driver holds the `Mapping` clone until the send completes. That clone is the
      lifetime guarantee.
    - Today the server copies every served byte from the pack into a frame `Vec` for
      `Effect::Send`. With this, a `Form::Raw` record is served with no user-space copy: the
      server sends a small header `Vec`, then a `SendMapped` of the value's range, in order.
    - Sim: deliver the bytes as `Send` does.
K2. **`Effect::RecvInto { conn: Conn, buffer: Vec<u8>, most: usize, tag: Tag }`, completing
    `Done::Read(buffer)` with up to `most` bytes appended after `buffer`'s existing contents.**
    - Empty appended means hung up, as for `Recv`.
    - Today each `Recv` returns a fresh `Vec`, and the client copies it into its frame buffer,
      so a value spread over many receives is copied twice.
    - With this, the client passes its buffer (reserved to the frame's length) and gets it
      back: one copy, kernel to final buffer, and no allocation per receive.
    - io_uring: a plain `recv` into `buffer.spare_capacity_mut()`. Multishot recv with provided
      buffers (store's request 6) still suits small asks on the server side.
    - Sim: append the delivered prefix.

> forwarded to core: `Effect::SendMapped` and `Effect::RecvInto` are lane core's (`crates/blaze-core/`); lane core is still running.

## 6. Workspace dependency `windows-sys` and blaze-core's Windows line (one change)

- What: in the root `Cargo.toml`'s `[workspace.dependencies]`, `windows-sys = { version =
  "0.61.2", features = ["Win32_Foundation", "Win32_Networking_WinSock", "Win32_Security",
  "Win32_Storage_FileSystem", "Win32_System_IO", "Win32_System_Ioctl",
  "Win32_System_JobObjects", "Win32_System_Memory", "Win32_System_Pipes",
  "Win32_System_SystemServices", "Win32_System_Threading"] }` (0.61.2 is in `Cargo.lock`), and in
  the same change, in `crates/blaze-core/Cargo.toml` (core's, but it must land with the
  workspace entry or every platform's manifest fails to parse):
  `[target.'cfg(windows)'.dependencies]` / `windows-sys.workspace = true`.
- Where: `Cargo.toml`, `crates/blaze-core/Cargo.toml`.
- Why: the IOCP driver (`crates/blaze-core/src/iocp.rs`, `iocp/`) and `fs.rs` on Windows use
  these modules. Until then a Windows build of `blaze-core` fails; Linux and macOS are not
  touched.

> applied: `windows-sys` 0.61.2 with those features was in the root `[workspace.dependencies]`; lane core is DONE, so `crates/blaze-core/Cargo.toml` now has `[target.'cfg(windows)'.dependencies]` `windows-sys.workspace = true`.

## 7. docs/v2/plan/lanes/core.md "Windows": how a process's end is heard

- What: say "a thread-pool wait on the process (`RegisterWaitForSingleObject`) posts its end to
  the port; the job's messages, which Windows does not promise to deliver, only say the job went
  past its memory" and "a memory-limited job's peak is at most its limit: Windows refuses any
  commit past it", in place of the job's `EXIT_PROCESS` message.
- Where: `docs/v2/plan/lanes/core.md` (coordinator's).
- Why: a lost `EXIT_PROCESS` would leave a spawn in flight forever; the wait always fires.

> forwarded to coordinator: `docs/v2/` is the coordinator's, not among the paths `requests` applies.

## 8. For the daemon (cargo or ci): start every process through a driver

- What: no `std::process::Command` in the daemon once the drivers run its processes.
- Why: on Windows, a child's pipe ends are inheritable between their creation and
  `CreateProcessW`; std's spawn inherits every inheritable handle, so a std spawn in that window
  holds another spawn's pipe open until it exits. On Unix the same holds for any descriptor
  without close-on-exec; the drivers set it on all of theirs.

> forwarded to cargo: the daemon's processes are lane cargo's (`crates/blaze-cargo/src/serve/`).

## From speed-runtime: `docs/v2/plan/lanes/core/plane.md`, "Memory admission" (2026-10-29)

- What: replace "`arena::Counting` is the daemon's global allocator (per-thread counts, sharded
  totals, no lock)" with "`arena::Counting` is the daemon's global allocator, over the allocator
  it uses (per-thread counts added to one total each time they drift 256 KiB and at thread
  exit: no lock and no atomic per allocation; `held()` is one load, within 256 KiB per other
  thread)". In "What others wire", the daemon line can say `Counting<Jemalloc>` (or mimalloc)
  rather than `Counting<System>`.
- Where: `docs/v2/plan/lanes/core/plane.md` (core's).
- Why: `crates/blaze-graph/src/arena.rs` changed so (see `docs/v2/plan/lanes/core/speed-runtime.md`),
  and the doc is `current`-kept by core.

> forwarded to coordinator: lane core is DONE and its doc falls back to the coordinator's `docs/v2/`, not among the paths `requests` applies.

## From speed-runtime-plane: one output buffer per caller of the plane (2026-10-29)

`Plane` gained `step_into(&mut self, PlaneInput, &mut Vec<PlaneOutput>) -> Result<(), Error>`
and `cancel_into(&mut self, u64, RequestId, &mut Vec<PlaneOutput>) -> Result<(), Error>`: they
append to the caller's buffer (nothing on an error). `step` and `cancel` are unchanged and still
allocate one `Vec` a step. A caller that keeps one buffer and drains it allocates nothing per
step.

1. **`crates/blaze-graph/src/machine.rs` (core).** What: give `Machine` a field
   `said: Vec<PlaneOutput>` (`Vec::new()` in `new`), and in `step` replace
   `let outputs = self.plane.step(input)?;` with
   `let mut said = std::mem::take(&mut self.said); let taken = self.plane.step_into(input, &mut said);`
   then, on `Ok`, map `said.drain(..)` exactly as `outputs.into_iter()` is mapped now, and put
   `self.said = said;` back before returning `taken` / the outputs. Why: one fewer allocation per
   step of every build-script and name-resolution graph; outputs unchanged.
2. **`crates/blaze-graph/src/exec.rs` (speed-runtime-pool, told in its window).** What: give the
   owner a `said: Vec<PlaneOutput>`; in `Owner::step` call `self.plane.step_into(input, &mut said)`
   on a buffer taken out of `self` (`std::mem::take`), and in place of `outputs(request, Result<Vec<_>>)`
   take `Result<(), Error>` and `for output in said.drain(..) { self.output(output) }`, then put
   the buffer back; the same for `Msg::Cancel` with `cancel_into`. Why: the scheduler's owner
   thread steps the plane once per task; this takes its one remaining allocation per step.
3. **`docs/v2/plan/lanes/core/plane.md` (core), "The plane".** What: add a item 7: "**Layout.**
   Nodes and tasks are `u32` slots into hot rows (64 bytes a node, 32 a task) and colder columns,
   reused through free lists; a request's deps are one edge array (a node's deps a range of it,
   its dependents a list threaded through it); task members a list through the node rows; ready
   sets 4-ary heaps whose nodes know their place; keys index the flight table by their own first
   eight bytes. In steady state a task costs the plane no allocation; `Plane::step_into` appends
   to the caller's buffer." Why: the doc is `current`-kept by core and now says nothing of how
   the plane holds its state (see `docs/v2/plan/lanes/core/speed-runtime-plane.md`).

> applied: lane core is DONE. 1: `crates/blaze-graph/src/machine.rs` `Machine` keeps `said: Vec<PlaneOutput>` and `step` drains `Plane::step_into` into it, mapped as before. > forwarded to speed: 2 (`exec.rs`) is speed-runtime-pool's, told by the requester. > forwarded to coordinator: 3, `docs/v2/plan/lanes/core/plane.md`, falls to the coordinator's `docs/v2/` with core DONE.

## From speed-runtime-vfs: one effect to look at many paths, and the kind of a change (2026-10-29)

The watcher (`crates/blaze-vfs/src/watch.rs`) now asks a reported path's `Effect::Stat` and
`Effect::ReadAll` together, so a batch is one round trip, but still two effects per path.

1. What: `Effect::Look { files: Vec<FileName>, tag }`, done as `Done::Looked(Vec<Looked>)` in
   the same order, `enum Looked { Gone, Dir, File { meta: Meta, bytes: Vec<u8> }, Failed(IoError) }`.
   Linux: one io_uring submission of linked `statx` + `openat` + `read` per path; macOS and
   Windows: one pool job walking the list. Why: a change batch of N paths becomes one effect
   and one completion instead of 2N, with no read of a path the stat says is gone or a
   directory (Watchman's and git's fsmonitor's batching).
2. What: in `Done::Changed`, each file with what the system said of it, `(FileName, Kind)`
   with `enum Kind { Removed, Dir, Changed }` (fanotify's `FAN_DELETE`/`FAN_ONDIR`, FSEvents'
   `ItemRemoved`/`ItemIsDir`). Why: a removal or directory event needs no stat at all; only
   `Changed` paths are looked at, as the brief asks ("no per-file stat where the change already
   says what it is").

The watcher moves to both once they exist; until then it stays correct with Stat + ReadAll.
