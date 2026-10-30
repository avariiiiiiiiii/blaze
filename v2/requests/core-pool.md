# Requests from lane core-pool

## 1. `Cargo.lock`: blaze-graph's new dependencies

- What: `crates/blaze-graph/Cargo.toml` now depends on `crossbeam-deque` (workspace) and, on
  unix, `libc` (workspace). Both are already in `Cargo.lock` (crossbeam-deque 0.8.6, libc
  0.2.186); only blaze-graph's `dependencies` list in the lock gains the two names.
- Where: `Cargo.lock`, the `blaze-graph` package entry.
- Why: the worker pool's deques (lock-free push, pop and steal) and the topology's
  `sched_getaffinity` / `sched_setaffinity` (Linux) and `sysctlbyname` /
  `pthread_set_qos_class_self_np` (macOS).

> applied: nothing to edit by hand: `crossbeam-deque` and `libc` are root workspace dependencies and both packages are in `Cargo.lock`; the first compile adds the two names to `blaze-graph`'s entry (no new package).

## 2. One `Cancelled` for the query database and the pump (lane db, coordinator)

- What: `blaze_query::Cancelled` (crates/blaze-query/src/lib.rs) becomes a re-export of
  `blaze_graph::Cancelled` (`pub use blaze_graph::Cancelled;`), replacing its own struct.
- Where: `crates/blaze-query/src/lib.rs`.
- Why: `Cx::get`'s "reused, waited on if running, or run inline" is
  `blaze_graph::pump::Flights::get_or_run`, which returns `blaze_graph::Cancelled`; the
  scheduler's `Work` returns it too. Two types for one thing would need a conversion at every
  `?`.

> applied: by db (`pub use blaze_graph::Cancelled;` in `crates/blaze-query/src/lib.rs`).

## 3. docs/v2/plan/lanes/core/plane.md "The executor" (core)

- What: say what the executor now is: groups are the CPUs of a level 3 cache or, where the
  machine says so (a cluster of more than one core), a cluster, the smaller winning; on macOS a
  group is an L2 cluster of one performance level (not "the performance and efficiency
  cores"), level 0 fast, and a job near no group goes to the least loaded group, fast first.
  Name `blaze_graph::pump` as #1301's rule (pump while waiting, cancel the wait that closes a
  cycle) and `exec::Scheduler` as the owner thread. `drive` runs a build's work and IO nodes on
  two pools lent to it alone and kept for the next build of their size, so a build starts no
  threads.
- Where: `docs/v2/plan/lanes/core/plane.md` (core's path).
- Why: docs-say-what-they-are; the section was written before the pool.

> applied: by core (`docs/v2/plan/lanes/core/plane.md`, "The executor").
