---
status: plan
summary: mem-core - blaze-core's I/O operations, timers and watch slots and blaze-graph's plane tasks onto blaze-mem, memory admission reading blaze-mem's counters (each task kind a tag, each tag a budget, a tag over budget throttled first), and the counters on the agent API; each cost against its bound.
date: 2026-10-29
---

# mem-core: the core and the plane onto blaze-mem

Part of lane mem ([memory.md](../../design/memory.md)). Paths: `crates/blaze-core/`,
`crates/blaze-graph/`, `crates/blaze-api/` (lanes core and ci, and every speed agent there, DONE).
Nothing was built or run; the tests are written beside the code for the coordinator's run.

Children, each with its own notes under [core/](core/):

| child | paths | notes |
|---|---|---|
| `mem-core-linux` | `blaze-core/src/{uring,watch,fanotify,cgroup}.rs` | `core/linux.md` |
| `mem-core-macos` | `blaze-core/src/{kqueue,fsevents,footprint}.rs` | `core/macos.md` |
| `mem-core-windows` | `blaze-core/src/iocp.rs`, `iocp/` | `core/windows.md` |
| `mem-core-sim` | `blaze-core/src/{sim,threads,net,process,fs,journal,tests}` | `core/sim.md` |
| `mem-core-plane` | `blaze-graph/src/{plane,plane_tests,machine,pool,pump}` | `core/plane.md` |

Mine: `blaze-core/src/wheel.rs`, `blaze-graph/src/{exec,admission,arena}.rs`, `blaze-api`.

## Timers: the wheel on a slab

`blaze_core::wheel::Wheel<T>` keeps its 704 slot lists (11 levels of 64) and occupancy bitmaps,
and its timers move from two parallel `Vec`s with a free list to one
`blaze_mem::Slab<Timer<T>>` tagged `Io` (at most `MAX_TIMERS` = 2^22). A timer's
`wheel::Handle` is its slab handle's bits, so a cancel or deadline asked with a handle whose timer
fired or was cancelled, and whose slot now holds another timer, is detected by the generation and
answers `None`: never another timer's value. The lists are intrusive (`prev`/`next` as handle
bits), so arm, cancel and fire touch only the timer and its list's ends.

Tests (in `wheel.rs`): the 400-seed model test against a sorted map now also runs the old wheel
(`wheel/reference.rs`, a verbatim copy, test-only) on the same operations and compares every
answer; a stale handle after reuse; a full slab is an error naming `io`; 10,000 warm arm-fire
rounds (a far timer armed and cancelled, a near one fired) under
`blaze_alloc_count::assert_at_most(0, ..)`.

## The scheduler's runs

`exec`'s run table was a list of leaked 64-slot chunks with a `Vec<u32>` free list. Now:

- each run's slot (`Run`: the pool's job header, the context, the `Live` it runs) is a
  `blaze_mem::Pool<Run>` slot tagged `Plane` (`Ctx::slots`, at most `MAX_RUNS` = 2^16): a pool
  because the slot is read by the thread that runs it while the owner keeps taking others, so it
  must never move and must not sit in a slice the owner holds `&mut` to;
- the owner names runs by `Handle<Slot>` in a `Slab<Slot>` (also `Plane`): the `Pooled` slot, its
  tag, pool and place. A `Done` carries the handle's bits, so a `Done` naming a freed run panics
  naming a stale handle before anything reads the slot;
- the owner takes and recycles slots through one `Cache`, so a warm run makes no call to the
  global allocator (test `a_warm_run_and_its_admission_allocate_nothing`, 10,000 take, admit,
  name, free and end cycles, budget 0);
- the mailbox and the IO queue are `crossbeam_channel::bounded` (`MAILBOX` = 4096 and
  `MAX_RUNS` + the IO threads), which allocate nothing a message where `unbounded` allocates a
  block every 31. The owner never sends to its own mailbox (a node it cannot run fails through
  `Owner::later`), so the owner cannot block on itself.

## Memory admission on blaze-mem's counters

**Blaze's own memory** is `arena::own()` = `arena::held()` (the global allocator's) +
`blaze_mem::tag::snapshot().committed` (every region's committed bytes). `machine_budget` takes
what others use as `memory.current` less `own()`, and the scheduler's `Held` input after each
`Done` is `own()`. Test `blaze_holds_what_blaze_mem_committed_too`.

**A task's peak** is what `arena::measure` gave (the global allocator inside the task) plus the
most its frames held at once. The frame half waits on lane mem: asked by tmux on 2026-10-29 for
`blaze_mem::frame::measure<R>(f) -> (R, usize)`, the most bytes the calling thread's frames held
at once while `f` ran (nested tasks on top of their enclosing ones), nesting, allocation-free.
Once it lands, `exec::measured` is
`let ((did, bytes), frames) = blaze_mem::frame::measure(|| arena::measure(|| arena::task(work)))`
with `peak = bytes + frames as u64`. Until then a task's frames are counted in its tag's
`task_peak` (blaze-mem's `task_ended`) and in `committed`, not in its predicted peak.

**Each task kind is a tag** (`admission::tag_of`), by the ranges of `blaze_query::kinds` and the
daemon's own kinds (`admission::kinds`, which `blaze-cargo`'s `serve/plane.rs` is asked to use):

| kinds | tag |
|---|---|
| `FILE`, `DIR`, `CONFIG` (1 to 3); units loads (`0x6000..0x7000`, `PLAN`) | `vfs` |
| `FACTS` (`0x1000..0x2000`); front `0x2200..0x2400` (names) | `nameres` |
| front `0x2000..0x2200` (syntax) | `parse` |
| front `0x2400..0x2c00` (signatures, bodies, inference) | `infer` |
| front `0x2c00..0x3000` (macros) | `expand` |
| front `0x3000..0x4000`; `LOWER`, `UB_TESTS`, `EVALUATE` | `mir` |
| back `0x4000..0x4800`; `LIBRARY`, `COMPILE`, `REACH` | `codegen` |
| back `0x4800..0x5000` | `link` |
| `USER` (`0x5000..0x6000`) | `usercode` |
| `FETCH` | `store` |
| `WALK` | `query` |
| anything else | `other` |

The kinds in use fit the front and back sub-ranges: `hir-def`'s names at `0x2200..0x2240`,
`hir-ty`'s at `0x2400..0x2600`, `blaze-codegen`'s at `0x4000..`, `blaze-units`' loads at
`0x6010..0x6800`. Lane db is asked to write the sub-ranges into `blaze_query::kinds`
(`v2/requests/mem-core.md`) so that a new kind is placed by its subsystem; the test
`each_kind_belongs_to_its_subsystem` pins the map.

**Each tag's budget** is its share of the machine budget in 64ths (`admission::share`,
`set_budgets`, called by `Scheduler::with_pool` when the plane has a budget):

| share | tags |
|---|---|
| 32/64 | `infer`, `codegen`, `link`, `frames` |
| 24/64 | `mir`, `query` |
| 16/64 | `nameres`, `intern` |
| 8/64 | `parse`, `expand`, `store`, `vfs`, `chunks`, `fuzz` |
| 4/64 | `usercode`, `other` |
| 2/64 | `plane`, `io`, `api` |

The shares add to 270/64: a budget is a cap on one subsystem, not a partition. The phases of a
build overlap little (a build is mostly inference, or mostly codegen and linking, at a time), so
each heavy subsystem may take half the machine, and the machine budget still bounds them
together. A subsystem past its share is a runaway (an interner that never frees, a query table
that keeps every revision), and is held back while the rest still fit.

**A tag over budget is throttled first** (`exec::Throttle`): when the plane runs a node, the
scheduler asks `tag::over_budget(tag_of(kind))`. Over budget, with another run of the tag under
way, the run waits in its tag's FIFO; it starts when a run of the tag ends and the tag is under
budget again, or when no run of the tag is left. So a subsystem whose memory sits in tables its
tasks do not free still has its tasks run, one at a time, and nothing waits for an end that never
comes. The plane counts a held run as running, its memory granted, so holding it back never lets
the plane grant more. At stop, held runs start whatever the budgets, so the table empties. The
queues are intrusive lists in a `Slab` (`Plane`), so holding and starting allocate nothing. Tests:
`the_throttle_matches_a_queue_per_subsystem` (200 seeds, against a per-tag `VecDeque` model),
`a_subsystem_over_its_budget_still_runs_one_at_a_time`.

## The counters on the agent API

`Request::Memory` is answered with `Response::Memory(Memory)` (schema 12): one `TagMemory` per
tag in `Tag::ALL`'s order (`tag`, `live`, `rate`, `budget`, `allocated`, `task_peak`,
`high_water`; declared in their keys' CBOR order, so the writer sorts nothing), plus `committed`
and `committed_high_water`. The rate is `tag::Rate` sampled at the session's clock
(`Session::tick`) since the last memory request. The session reads the counters through
`Daemon::memory: fn() -> Snapshot`, which the daemon sets to `blaze_mem::tag::snapshot`, so the
session stays a state machine and its test gives fixed counters. It goes over the socket and HTTP
like every request; there is no MCP. Tests: `memory_gives_each_subsystems_counters_and_rate` and the
codec identity test, which now generates both variants.

## Speed of light

| path | bound | where it lands |
|---|---|---|
| timer arm | one slab slot write (~48 B, one line) + one list tail link + one bitmap OR: ~3 lines touched, ~5 ns warm | the same: no search, no allocation |
| timer fire | `trailing_zeros` on the level bitmap + unlink + slab remove: ~3 lines, ~5 ns a timer | the same; a cascade moves each timer once a level, as before |
| run take/free | a cache pop + a slab insert, then a slab remove + a cache push: ~4 lines, ~10 ns | the same; the cross-thread floor is the job handoff (one line moved between cores, ~60 to 100 ns), which the slot does not add to |
| admission check | two relaxed loads on the tag's 128-byte line: ~1 ns uncontended, one line transfer (~60 ns) when another core just counted into the tag | the same; the throttle's arrays are owner-only |
| memory record | 19 × 5 relaxed loads (~19 lines) + the record (~1.5 KB of CBOR) | the same; asked rarely, so its 19 name strings are not worth interning |

## Open

- `blaze_mem::frame::measure` (lane mem), then the frame half of a task's peak in
  `exec::measured`.
- `v2/requests/mem-core.md`: the daemon's `cx!` sets `Daemon::memory`, and its kinds come from
  `admission::kinds`; `blaze-ui` renders `Response::Memory`; lane db writes the front and back
  sub-ranges into `blaze_query::kinds`.
