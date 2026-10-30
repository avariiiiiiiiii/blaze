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
