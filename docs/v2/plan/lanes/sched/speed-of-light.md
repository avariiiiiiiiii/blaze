---
status: plan
summary: lane sched's speed of light for the scheduler core (crates/blaze-graph/src/sched/): the cache lines and instructions each step must touch, what the code touches, and the bound for steals, wakes and cycle checks.
date: 2026-10-31
---
# sched: speed of light

Each step of the core (`crates/blaze-graph/src/sched/mod.rs`) does a fixed amount of work: it
reads and writes a few words. The bound for a step is the cache lines it must touch, plus, for
anything that crosses cores, one coherence transfer per line another core wrote.

## The steps

| Step | Lines it must touch | What the code touches |
|---|---|---|
| local spawn | the job's record, the queue's tail, the core's stealable bit | the job's 28-byte `Job` record (one line), the `VecDeque` tail, one bitmask word; `balance` reads `parked_count` (one line, shared) |
| pick from own slot or queue | the core's slot or queue head, the job's record | the same, plus one `Decision` pushed into a reused `Vec` |
| `done` with no waiters | the job's record, then a pick | the same; waiter and claim lists are `NONE` heads in the same record |
| `done` with k waiters | the record of each waiter (k lines) | k records, linked through their own `next` field (no side allocation) |
| suspend | the job's record, the target's record (to link) | the same, plus the cycle walk below |
| steal | a mask word per level, the victim's queue head | 4 levels × ⌈cores/64⌉ words ANDed (4 words on 256 cores), then the victim's `VecDeque` head |
| wake to a parked core | the parked mask, the target's slot | `nearest_parked`: the same mask walk, then one store |

At L1 latency (about 1 ns a line) and a few instructions a line, a spawn, a run and an end of a
job on one core cost 10 to 15 ns together in the core model, which is 70 to 100 million jobs a
second per core. Rayon's `join` on a warm deque costs about 10 to 20 ns, and Tokio's spawn and
run about 50 to 100 ns; the core's local path is the same order of work as Rayon's.

Across cores the bound is the coherence transfer: a steal moves the victim's queue head line,
which costs 50 to 100 ns within an L3 group and 150 to 300 ns across sockets. The nearest-first
order is what keeps steals on the cheap side. A wake of a parked core costs one futex wake in
the driver (1 to 2 µs), paid only when a core is parked. Work-conservation means that is also
the only time a core sleeps while work exists.

## Where the code is off the bound, and why

- **`least_loaded` scans every core**, but only for a spawn with no spawner that names a group,
  a node or `Any`: the driver's roots, a handful per build. It stops at the first parked core.
- **The cycle check walks the wait chain**: L record loads for a chain of length L, each likely
  a miss on another core's record. Chains are as deep as the query stack (tens to a few
  hundred). The walk costs nothing on the common path, because a wait whose target is running
  or queued stops after one load. A wait that is refused allocates its `Cycle`, the only
  allocation outside setup, on an error path.
- **One job per steal.** Chase-Lev runtimes steal half a deque at a time to amortise the
  transfer; the core is the decision model, and the sharded runtime (integration, in
  `docs/v2/plan/lanes/sched.md`) steals a batch of up to half the victim's queue under the same
  nearest-first rule.
- **The core is one state machine.** As a runtime it would serialise every step on one lock.
  The integration plan splits it by owner, so a run that nobody waits for touches only its own
  core's lines, and only waits take the wait table's lock. That is how the runtime reaches the
  per-step bound above on every core at once.

## Allocation

Zero on the warm path, checked by `a_warm_scheduler_allocates_nothing`. Every column is sized
at `Sched::new(cores, jobs)`. Queues are `VecDeque`s that keep their capacity, waiter and claim
lists are intrusive (`u32` links in the records), key flights are a reused free list beside an
`FxHashMap` sized for `jobs`, timers are `blaze_core::wheel::Wheel`'s slab, and the scratch
buffer and the decision buffer are reused.
