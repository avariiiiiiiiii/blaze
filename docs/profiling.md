---
status: plan
summary: blaze's one profiler (lane observe; a plan until each part it describes lands), one span stream into one buffer with several readers. The schema (spans, counters, builds, samples, provenance, inputs, alloc_samples, heap_by_owner; Arrow, defined once in crates/blaze-profile/src/schema.rs), the flight recorder, the Parquet history with `blaze debug sql` and `nix run .#duckdb`, the live feed (`blaze debug live`, the explorer), the stall watchdog, and `BLAZE_PROFILE` / `BLAZE_STATS`.
date: 2026-10-31
---
# Profiling blaze

blaze has one profiler, its own: one stream of span and counter events, one buffer, and
several readers.

- **The stream.** Every info-level `tracing` span of a `blaze` target, every rust-analyzer
  `*_query` span, and the counters blaze plots (memo hits and misses, the pool's queue, bytes
  of frames and of the heap). On Linux, also the CPU sampler's stacks.
- **The buffer.** The flight recorder keeps each thread's recent events in a fixed ring, in
  the schema below. It is always on, in every build of blaze, and allocates nothing and does no
  I/O per span.
- **The readers.**
  - The history: Parquet files the recorder flushes. `blaze debug sql` runs DuckDB over them.
  - The live feed: Arrow batches every 250 ms over the daemon's socket. `blaze debug live`
    renders them as a TUI, and the explorer at ix.dev/explore draws them.
  - The stall watchdog: it writes the buffer out when a request stops moving.
  - `BLAZE_PROFILE=<dir>`: a whole build's Chrome trace.

DuckDB is never linked into blaze. `blaze debug sql` runs the pinned DuckDB CLI.

## The schema

Defined once, in `crates/blaze-profile/src/schema.rs`. The Arrow schema of each table, the
Parquet files, the live feed, the DuckDB views and this page all come from it. Schema version
1. Each Arrow schema's metadata carries `blaze.table`, `blaze.schema` (the version) and
`blaze.clock`.

**Time.** Every `*_ns` instant is in nanoseconds on the machine's monotonic clock: Linux
`CLOCK_MONOTONIC`, macOS `CLOCK_UPTIME_RAW` (`mach_absolute_time`). It is the same clock in
every process on the machine, so the client's, the daemon's and the build scripts' spans line
up as written. The recorder reads the CPU's counter (`CNTVCT_EL0`, `rdtsc`) per event and maps
it to that clock when it writes, calibrating against the clock at both ends of the stretch
written. Durations are nanoseconds.

`dict` is a dictionary-encoded string (`u32` keys). `stack` is a list of `dict` frames,
outermost first. `?` marks a nullable column.

### `spans`: one row a span, from its start to its close, on the thread that made it

| column | type | |
|---|---|---|
| build_id | u64? | the build it was part of; null outside any build |
| commit | dict? | the commit of the workspace the build was of |
| process | u32 | the process id |
| process_name | dict | what the process is: blaze serve, blaze build, a build script's crate |
| thread | u32 | the operating system's thread id |
| thread_name | dict | the thread's name |
| span_id | u64 | unique in its process while the process lives |
| parent | u64? | the span it is inside; null at a root |
| depth | u16 | how many spans its thread was inside when it began |
| name | dict | the span's name |
| target | dict | its tracing target: the module path it was made in, unless it names one (`blaze` for blaze's steps) |
| start_ns | i64 | when it began |
| dur_ns | i64 | how long it was open |
| self_cpu_ns | i64? | its thread's CPU time less that of the spans under it; only while `BLAZE_PROFILE` is on |
| alloc_bytes | u64? | bytes its thread allocated while it was open |
| allocs | u64? | allocations its thread made while it was open |
| query_kind | dict? | the query it ran, for a span of a query |
| crate | dict? | the crate it was of, from its field `crate` |
| function | utf8? | the function it was of, from its field `function` or `subject` |
| unit | utf8? | the codegen unit it was of, from its field `unit` |
| frame_grow_copies | u64? | blocks its thread's task frames moved to grow them while it was open; null when not counted or closed on another thread |
| frame_copied_bytes | u64? | bytes those moves copied |

A `Vec` in a frame grows in place while it is the frame's last block. Once something is
allocated after it, it moves when it grows, copied whole. blaze-mem counts those moves per
frame in two `Cell`s, on the slow path only. The frame reports them at task end
(`tag::task_ended`), to the tag's totals (`Counters::grow_copies` and `grow_copied`) and to the
thread's running total (`tag::copied_on_thread`). A span's part is the difference across it,
noted only when the span closes on the thread it began on.
- A span that copied gets one extra ring row after its own, so a span that copied nothing
  costs nothing more.
- The columns are at the end of the table so that adding them keeps the schema version.
- `frame_copies_per_query_kind` ranks query kinds by bytes copied. That ranking decides
  whether blaze needs a second growth frame per worker.

### `counters`: one row a value of a counter at an instant

| column | type | |
|---|---|---|
| build_id | u64? | the build, or null |
| process | u32 | the process id |
| name | dict | what is counted: `memo hits`, `memo misses`, `joined`, `pool queued`, `pool running`, `frame bytes`, `live bytes`, and per job on a fiber `fiber stack reserved`, `fiber stack deepest`, `fiber stack touched`, `fiber stack grown` (bytes) |
| t_ns | i64 | when |
| value | f64 | the value then |
| fiber | u32? | the fiber a `fiber stack` counter is of, or null |
| job | u64? | the job that ran on that fiber, or null |

The fiber driver notes the four `fiber stack` counters when a job ends, as one row in the
recorder (`blaze_profile::fiber_stack`):
- `reserved` is the fiber's stack, guard page left out.
- `deepest` is how far below the stack's base the job went.
- `touched` is how much of the fiber's stack is committed, which only grows over the fiber's
  life.
- `grown` is the segments the job grew past the fiber's stack.

The `fiber_stacks` view has one row per job, and `jobs_by_stack_high_water` ranks a build's
jobs by `deepest + grown`. A later choice between fibers and a narrow stackless primitive
rests on these numbers.

### `builds`: one row a build request the daemon ran

| column | type | |
|---|---|---|
| build_id | u64 | the build's number, unique in the daemon's store |
| commit | utf8? | the workspace's commit, when it is a git checkout |
| workspace | utf8 | the workspace's root |
| command | utf8 | what was asked: build, test, run, check, with the arguments |
| started_ns | i64 | when the daemon took it |
| dur_ns | i64? | how long it ran; null while it runs |
| started_unix_ns | i64? | when it was taken, on the wall clock |
| outcome | dict | ok, failed, cancelled, stalled or running |
| hits | u64? | nodes the memo settled without running them |
| misses | u64? | nodes that ran their task |
| joined | u64? | nodes that took a task another node ran |

### `samples`: one row a CPU sample of a running thread

| column | type | |
|---|---|---|
| build_id | u64? | the build, or null |
| process | u32 | the process id |
| thread | u32 | the thread |
| t_ns | i64 | when it was taken |
| stack | stack | the call stack, outermost first, each frame demangled |
| span_id | u64? | the innermost span its thread was inside |

### `provenance`: one row a query of a build that was verified or rerun, with the cause

| column | type | |
|---|---|---|
| build_id | u64 | the build |
| query_key | utf8 | the query and its key |
| kind | dict | the query's kind |
| outcome | dict | verified, reran, or reran-same-output (an early cutoff) |
| cause | utf8? | the input or query key whose change made it rerun |
| dur_ns | i64 | how long verifying or running it took |
| unit | utf8? | the codegen unit it was for |
| crate | dict? | the crate it was of |

### `inputs`: one row an input that changed for a build

| column | type | |
|---|---|---|
| build_id | u64 | the build |
| key | utf8 | the input: a path, a variable's name, a flag, a dependency |
| kind | dict | file, env, flag or dependency |
| detail | utf8? | what changed about it |
| seen_by | dict | watcher, rescan or request |

### `alloc_samples`: about one allocation every 512 KiB allocated, on each thread

| column | type | |
|---|---|---|
| build_id | u64? | the build, or null |
| process | u32 | the process id |
| thread | u32 | the thread |
| t_ns | i64 | when |
| bytes | u64 | the bytes the sample stands for (the sampling interval) |
| stack | stack | the call stack, outermost first |
| span_id | u64? | the innermost span its thread was inside |

### `heap_by_owner`: the live heap at the end of a build, by what holds it

| column | type | |
|---|---|---|
| build_id | u64 | the build |
| owner | dict | what holds the bytes: a memo table per query kind, an arena |
| bytes | u64 | live bytes |

### `tiles`: a result, not stored

The explorer's zoomed-out view: the spans of a time range per (process, thread, depth,
bucket). Its columns are `process u32, thread u32, depth u16, t0_ns i64, t1_ns i64, count u32,
busy_ns i64, name dict` (the name of the longest span in the bucket).

### `open`: a result, not stored

The spans each live thread is inside now, which are not `spans` rows until they close: what the
live feed sends beside the rows that closed. Its columns are `process u32, thread u32,
thread_name dict, depth u16 (0 outermost), name dict, target dict, start_ns i64`.

## The flight recorder

Every blaze process keeps a fixed ring per thread, holding that thread's recent spans and
counter values as `spans` and `counters` rows. Recording allocates nothing and does no I/O. A
span's fields (`crate`, `function`, `unit`) are interned the first time each value is seen.
Next to it, on Linux, a sampler records every running thread's stack 49 times a second
through `perf_event_open`. Frames are named only when they are written out.

Each row is 64 bytes (8 words): close and start ticks, span id, parent, a word packing the kind,
callsite, depth, the ring that made the span and its allocation count, the interned crate and
function, the build and unit, and the bytes allocated. A ring holds 16384 rows (1 MiB). A span's
open state lives from its start to its close in a slot of the ring of the thread that made it.
Its id names that ring, slot and a sequence number, so a span closed on another thread still
finds where it began. Readers take rows with a cursor (`flight::read`): only what is new, and a
reader slower than the writers skips what was written over, counted.

## The live feed

`blaze debug live` and the explorer's live stream each get a `live::Feed` of their own on the
daemon. Every 250 ms it takes what the rings got since its last frame, plus the `open` table,
and sends a frame: `at_ns`, `skipped`, `lost`, then the `spans`, `counters` and `open` tables,
each as one Arrow IPC stream compressed with zstd. A subscriber that can't take a frame on time
misses that tick. Its cursor keeps its place, and rows written over meanwhile show up as
`skipped`, so the daemon holds at most one frame per subscriber. The client folds frames into
`live::Top`, which renders:
- each thread's running spans, outermost first, with how long it has been inside them;
- the ten longest-running spans;
- spans closed per second;
- the memo's hit rate from the last build's counts;
- a warning when nothing has closed for 10 s while threads are inside spans (the watchdog
  fails the request at 60 s).

## The stall watchdog

The daemon counts progress: spans closed, query runs and tasks ended. When tasks are running
and none of these has moved for the bound (60 s), it:

1. writes `state/stalls/<time>.json`: the flight recorder's last two minutes and the CPU
   samples as a Chrome trace (ui.perfetto.dev opens it);
2. fails each running request with the error `blaze stalled: no span closed and no task ended
   for N s while M tasks ran`, followed by each thread inside spans, outermost span first, and
   how long it has been in each;
3. stops the daemon, whose threads may be deadlocked; the next request starts a new one.

A deadlock (jobs that all wait on each other, as in blaze-pool's hang at 0% CPU) becomes this
error within the bound instead of a request that never ends.

## `BLAZE_PROFILE` and `BLAZE_STATS`

`BLAZE_PROFILE=<dir> blaze build` writes, for every process of the build, its spans with each
`blaze` span's own CPU time. At the end they are merged into `<dir>/trace.json` (a Chrome
trace), `phases.tsv`, `functions.tsv` and `passes.tsv`. `BLAZE_STATS=1` prints the build's wall
time by step.
