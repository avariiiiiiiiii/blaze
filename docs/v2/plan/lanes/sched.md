---
status: plan
summary: lane sched (2026-10-31): a waiting job never blocks its thread, and the scheduler is a sans-IO state machine. Phase 1 is the synchronous core in crates/blaze-graph/src/sched/ with its property tests; phase 2, how a job suspends (stackful fibers or async queries), waits on Andrew, with what each would change counted here.
date: 2026-10-31
---
# sched: a scheduler where waiting holds no thread

Andrew, 2026-10-31 18:24: a waiting job never blocks its thread, and the scheduler is a sans-IO
state machine. It lands after the self-build and the cargo bench, and until then nothing of
`blaze_graph::pool`'s API or its callers changes.

## Why

Today a wait inside a task **pumps** (`crates/blaze-graph/src/pump.rs`): the waiting thread runs
other jobs on top of its own stack, up to `PUMP_DEPTH` (16) deep, and a wait that would close a
cycle, counting the edge from each pumped job down to the one under it, is `Cancelled`. That
has three costs:
- a job pumped above a waiter pins the waiter until it returns, even when the waiter's value is
  ready;
- the cycle check has to count thread-stack edges, so it refuses waits that no data cycle
  forces, and the refused task runs again from the start;
- a second pool that blocks (rayon, before lane one-pool removed it) deadlocks against it.

A job that suspends, freeing its core, removes all three: no stack edges, no pumping, no
blocked thread.

## Phase 1: the core (written)

`crates/blaze-graph/src/sched/` (`mod.rs`, `cores.rs`, `tests.rs`), module
`blaze_graph::sched`. It is synchronous: no thread, no clock read, no sleep, no IO. It has no
caller yet, and `pool`, `pump`, `par` and `exec` are untouched.

| Input | Meaning |
|---|---|
| `spawn(job, parent, Affinity)` | a new job, from a running job or the driver |
| `claim(key, by)` | `by` (a job, or `None` for IO or a child process) makes `key`: single flight, `false` if it is taken already |
| `suspended(job, Wait)` | the running job waits for a job, a key's flight, or a time; `Ready` if that is done already, `Err(Cycle)` if it would close a cycle |
| `done(job)` | the running job ended, and with it every key it claimed |
| `key_done(key)` | a flight ended |
| `core_idle(core)` | the core's thread is free (started, woken, or spurious) |
| `advance_clock(now_ns)` | the driver read the clock |

| Output | Meaning |
|---|---|
| `take_decisions()` → `Run { core, job }` | the free core runs (or resumes) the job now |
| `take_decisions()` → `Wake { core, job }` | the job is a message to the core's inbox; a parked core is unparked and gets `Run` at its `core_idle` |
| `next_deadline()` | when to call `advance_clock` next |

The topology is an input, `Cores::new(Vec<CoreInfo { smt, l3, node }>)`, checked to be nested
(SMT siblings share an L3 group and node; an L3 group is on one node), and never read from the
machine inside the core. A driver builds it from `topology::Topology` and the SMT sibling lists.

### Policy

- **One queue per core; a job runs where it was spawned.** A child goes on its spawner's
  queue. The owner pops the newest (depth-first, as Chase-Lev and Rayon) and a thief takes the
  oldest. `Affinity` can say otherwise: a core, an L3 group, a node, or the least loaded core.
- **A waiter wakes on the core that finished what it waited for**, in that core's LIFO slot
  ahead of its queue, because that core's cache holds the value it reads. After `LIFO_RUNS`
  (3) slot runs in a row the core takes its inbox and queue first (Tokio's rule), so a chain of
  waiters cannot starve the queue. For a flight of the driver's (IO), "the core that finished"
  is the core the waiter last ran on.
- **Cross-core wakeups are messages**: a job handed to another core goes into that core's inbox
  (`Wake`). Inbox, queue and slot are all stealable.
- **Work-conserving, nearest first.** While any core is parked, no job sits stealable: a job
  made runnable on a busy core goes at once to the nearest parked core (its SMT sibling, then
  its L3 group, then its node, then anywhere). A core with nothing of its own steals in the same
  order. Within a level the search starts at the core after the thief, so thieves spread out.
  One consequence: a steal crosses a node only when the thief's node has nothing runnable.
- **Cycles are refused when they are asked for.** A job waits on one thing at a time, so the
  waits form a graph where each job has at most one outgoing edge, and checking a new wait is a
  walk along one chain: through `Wait::Job` to that job, through `Wait::Key` to the flight's
  claimer, stopping at a running or queued job, a timer, or a driver's flight. A wait that
  would close a cycle returns `Cycle { jobs }` in wait order (the asker first), and changes
  nothing.

### Invariants and their tests

`sched/tests.rs` drives the core with a simulated driver over random interleavings drawn by
Hegel: spawns with random hints, parallel maps, waits on jobs, keys and times, claims, ends,
key flights by jobs and by the driver, `core_idle` (requested and spurious), and clock
advances, on random nested shapes of up to 3 nodes × 2 L3 groups × 3 cores × 2 SMT threads.
After every input, `Sched::audit` checks that every structure agrees and that no job is
stealable while a core is parked; the driver checks each decision against the state.

| Test | Property |
|---|---|
| `every_runnable_job_runs` | no lost wakeup: once nothing new arrives, every job ends, each after a `Run`, and every core parks |
| `no_deadlock_when_waits_form_a_dag` | waits that form a DAG (ranks drawn at random), including parallel maps whose items wait for other jobs (the rayon case), all end on 1 to 3 cores |
| `a_cycle_of_waits_is_reported_naming_its_jobs` | arbitrary waits: each refusal names exactly the cycle that the model's own wait graph closes, in order, and no wait that closes no cycle is refused. With cycles refused, every job still ends |
| `a_steal_never_crosses_a_node_while_its_own_has_work` | on shapes of two or more nodes, every steal or handoff across a node happens while the thief's node holds no stealable job (counted by a scan at the moment of the steal) |

The unit tests pin each rule: `a_job_runs_where_it_was_spawned`,
`a_waiter_wakes_on_the_finishing_core_ahead_of_its_queue`, `a_waiting_job_frees_its_core` (one
core, a job waits for its own child), `an_idle_core_steals_its_sibling_then_its_l3_then_its_node_then_anywhere`,
`a_job_made_runnable_goes_to_the_nearest_parked_core`, `a_cross_core_wake_is_a_message_to_its_inbox`,
`a_key_flight_ends_with_its_job`, `a_cycle_names_its_jobs_in_wait_order`,
`a_timed_wait_wakes_at_its_deadline`, `the_slot_gives_way_to_the_queue_after_its_runs`,
`a_shape_that_is_not_nested_is_refused`, and `a_warm_scheduler_allocates_nothing` (spawns,
runs, steals, waits, flights and timers, measured with `blaze-alloc-count`: zero allocations).

### Choices made

- **Key waits mean "the key's current flight".** A wait on a key with no flight is `Ready`: the
  job re-reads the memo and, finding nothing, claims the key itself. That is the single-flight
  protocol `pump::Flights::get_or_run` already follows, and it closes the race between reading
  the memo and the flight ending without the core remembering every key ever done.
- **A job's end ends its flights.** A claimed key whose job ends without `key_done` wakes its
  waiters anyway; they find no value in the memo and the plane reports the job's error. Waiters
  are never left asleep.
- **Timed waits** use `blaze_core::wheel::Wheel` (so `blaze-graph` now depends on `blaze-core`;
  `blaze-core` depends on neither `blaze-graph` nor anything that does).
- **Contract violations panic** (a `done` for a job that isn't running, a wait for a job never
  spawned): they are driver bugs, and the message names the job.

### Integration (after the self-build and the cargo bench)

The core is the specification and the simulator. The runtime keeps the same state split by
owner: each worker holds its core's slot, queue and its job records while they run; an inbox is
a multi-producer queue that other cores can also steal from (`crossbeam_deque::Injector`, one per
core); "parked" and "stealable" are the same bitmasks, as atomics. The wait table (per-job
`wait`, waiter lists, flights) is the one structure several cores write: it stays behind the
one lock `pump::WAITS` takes today, and only a wait or a woken flight takes it. A run that nobody
waits for touches only its own core. `pool::Pool`'s API (`submit`, `submit_next`,
`submit_critical`, `scope`, `install`) maps onto `spawn` with `Affinity`, and `par` onto spawn
plus a join that suspends. The pump, `PUMP_DEPTH` and the thread-stack edges are then deleted.
How "suspend" happens is phase 2.

## Phase 2: how a job suspends (waits on Andrew; not started)

Counted on 2026-10-31 in this tree with `rg`; the files and counts are the ones a reviewer can
rerun.

### What both share

- `crates/blaze-graph/src/pump.rs` (1430 lines): pumping, `PUMP_DEPTH`, the per-thread `Stack`
  and the thread-stack edges go; `Flights` keeps single flight and calls `Sched::claim` /
  `suspended` / `key_done`.
- `crates/blaze-graph/src/pool.rs` (2113 lines): the worker loop becomes `core_idle` → `Run` →
  resume, and parking follows `Wake`.
- `crates/blaze-graph/src/par.rs` (829 lines): a scope's join suspends on its items.
- `crates/blaze-graph/src/exec.rs` (1936 lines): submit becomes spawn.
- The query engine's wait sites, the only places a task waits today: 32 matches of
  `get_or_run` / `run_if_idle` / `wait_until` / `pump::wait` in 3 files, two of them in the
  query engine (`crates/blaze-query/src/db/function.rs:324`,
  `crates/blaze-query/src/lib.rs:2442-2444`).
- Per-run state kept in thread-locals across a query call has to become per-job, because a job
  can resume on another core: `crates/blaze-query/src/lib.rs` (`INSIDE`, `STACKS`,
  `BUFFERS`, `UNREADABLE`), `crates/blaze-query/src/db/local.rs` (`LOCAL`: the active
  frames, edges, captures), and `crates/blaze-query/src/segment.rs` (`LANE`, which is fine as
  long as it is read per write and never held across a wait). The tree has 163 `thread_local!`
  in 130 files; the rest are scratch buffers used within one call with no wait inside, to be
  audited for exactly that.

### A: stackful fibers (corosensei)

A job runs on a stack of its own; `suspended` switches back to the worker's loop, and `Run`
switches into the job's stack.
- **Query call sites changed: 0.** The 462 `#[blaze_query::tracked]` functions, the 4099 call
  sites that pass or go through a `db` (in 491 files) and the 3852 functions taking `&Db` (in
  448 files) stay as they are. The sync trait code that calls queries also stays: the
  next-solver's `impl Interner for DbInterner` (`crates/hir-ty/src/next_solver/interner.rs`, 97
  database reads across `rustc_type_ir`'s 117-method `Interner`), the vendored
  `rustc_type_ir` and `rustc_next_trait_solver` (2093 functions in 80 files), and 107
  fold/visit/relate/iterator impls in hir-ty, hir-ty-mir and hir-def.
- **New code**: a stack pool (reserved, guard page below, committed as touched, reused, from
  `blaze-mem` by the memory rules) and the switch. corosensei would be a new workspace
  dependency; `crates/blaze-check/src/coro.rs` already has a few-instruction switch for x86-64
  and aarch64 Unix and fibers on Windows, which could be lifted into a shared crate instead
  (one way per job).
- **Cost**: two switches per wait (about 10 to 20 ns each) and one stack per suspended job.
  Admission bounds how many are suspended at once. The 57 `stacker` / `ensure_sufficient_stack`
  sites that guard deep recursion work on any stack. Today each worker has a 64 MiB stack.
- **Risk**: the type system cannot see a fiber switch. Anything `!Send` held across a wait
  (an `Rc`, a `RefCell` guard, a reference into a thread-local) moves threads when the job
  resumes elsewhere. The guard is that waits happen only inside the query engine's
  `get_or_run`, so the audit is "what is borrowed across a query call", plus a debug check that
  a job resumes with no thread-local borrow open.

### B: async queries

Every query becomes a future; `suspended` is `Poll::Pending`, and a wake is a `Waker`.
- **Query call sites changed: about 4100.** All 462 tracked functions become `async`. The 4099
  call sites in 491 files gain `.await`, and their callers turn async in turn: up to the 3852
  functions taking `&Db` in 448 files, most of the front end.
- **Sync trait boundaries that cannot await**: `rustc_type_ir`'s `Interner` (117 methods; the
  97 database reads in `interner.rs`), and the solver in `rustc_type_ir` and
  `rustc_next_trait_solver` (2093 functions, 80 files) that calls it. They would need an async
  fork of the solver, or every fact fetched before solving, which gives up laziness and changes
  which queries run. Add the 107 fold/visit/relate/iterator impls.
- **Recursion**: queries recurse (inference asks queries that infer), and a recursive async fn
  needs its future boxed, so a box or an arena slot per query call.
- **Parallel loops**: the 114 `par::` calls in 36 files take async closures.
- **What it buys**: the compiler checks `Send` across every `.await`, there are no stacks to
  size, and dropping a future cancels it.

### The numbers side by side

| | Fibers | Async queries |
|---|---|---|
| Query call sites edited | 0 | ~4100 in 491 files |
| Functions whose signature changes | 0 | 462 queries + up to 3852 `&Db` functions |
| Sync solver code that must change | none | `Interner` (117 methods) + 2093 solver functions |
| Executor files | pump, pool, par, exec (4) | the same 4 |
| Thread-locals moved to per-job | 3 files in blaze-query | the same 3 |
| Checked by the compiler | no (audit and a debug check) | yes |
| Memory per suspended job | a stack (committed pages only) | the future, boxed per recursive call |

The choice is Andrew's. The counts lean towards fibers: one executor change and three
thread-local moves, against an edit of most of the front end plus an async fork of the trait
solver.
