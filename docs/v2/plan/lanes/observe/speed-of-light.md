---
status: plan
summary: lane observe's speed of light: what the always-on flight recorder, the stall watchdog and the CPU sampler in blaze-profile cost per span, per plot, per sample and in memory, the bound the hardware sets, and how to measure that together they stay under 1% of a warm no-op build and of a test build.
date: 2026-10-31
---
# Lane observe: speed of light

The bar: the recorder and the sampler together cost under 1% of a warm no-op build and of a
test build, so they stay on in production. The counts here are read from the code in
`crates/blaze-profile` (nothing was built or run for this pass); the bounds are arithmetic. The
measurement that turns them into numbers is at the end.

## A span

What an info span of a `blaze` target (or a rust-analyzer `*_query` span) does, without
`BLAZE_PROFILE` and without the feature `tracy`:

| step | work |
|---|---|
| open (`opened`) | hash the callsite's address, probe the site table (1 load, rarely 2), read the clock, test `on()` (1 load) |
| enter (`flight::entered`) | thread-local read, load and store the depth, store the id |
| exit (`flight::exited`) | thread-local read, load the depth, compare the top, store the depth |
| close (`flight::closed`) | thread-local read, read the clock, a release fence, 2 relaxed stores and a release store (the entry), a relaxed add (the progress count) |

On top of that, `tracing` itself makes 4 dynamic calls through the global dispatcher and checks
the callsite's cached interest. Nothing allocates and nothing locks; the site table is filled
once per callsite (`register`, under a lock, at interest time).

The bound: two clock reads plus about ten loads and stores to one thread's own cache lines plus
one fence. On aarch64 the clock is `CNTVCT_EL0` (a few cycles) and the fence is a `dmb ish`
(tens of cycles); on x86_64 it is `rdtsc` (about 25 cycles each) and the fence is free (x86
stores are already release). Expected: 15 to 30 ns a span on Apple M-series, 25 to 50 ns on
x86_64, most of it `tracing`'s dispatch and the fence.

## A plot

`flight::plot`: thread-local read, clock read, the same entry write as a close. About 5 to 10 ns.
The daemon plots 4 values a loop turn and 3 a build (the brief's counters), so plots never
matter.

## Memory

Each thread that records holds one ring of `CAP` = 32768 entries, 16 bytes each: 512 KiB of
address space. It is allocated zeroed and never written up front, so a page is committed only
once entries reach it: a pool thread that records a few spans holds a few pages. A thread that
records steadily commits the whole 512 KiB after 32768 entries. With 64 such threads that is
32 MiB, which is the ring's real cost; at 10,000 spans a second a ring holds the last 3 s. The
rings of threads that ended are reused, never freed, so the total is bounded by the most threads
alive at once. Halving `CAP` halves both the memory and the window.

The site table is 4095 slots of 8 bytes (32 KiB, static). The open-span stack is 32 ids per ring.

## The watchdog

It runs on the daemon's loop, once a turn: one lock of the ring list and a sum over the rings'
progress counts (one load each), then compares. The loop turns at most every millisecond while
tasks run, so it costs about 100 loads a millisecond with 100 threads, nothing next to a turn.

## The CPU sampler

On Linux, `perf_event_open` with a task-clock event at 49 Hz on each thread: the kernel takes
each sample (registers plus a frame-pointer call chain, user space only) into the event's
mmap ring. A sample costs a few microseconds of the sampled thread, so 49 a second is about
0.025% of a busy thread. The sampler thread wakes every 100 ms to drain the rings and to list
`/proc/self/task` for new threads (one `readdir`, tens of microseconds for 100 threads).

On macOS (aarch64), the sampler thread does it from user space 49 times a second: for each
thread holding a ring, `thread_info` (is it running), and for each running one,
`thread_suspend`, `thread_get_state`, a frame-pointer walk within the thread's stack bounds, and
`thread_resume`. The sampled thread is stopped for about 10 to 20 µs a sample, about 0.1% of a
busy thread. The sampler's own thread costs about 1 µs a thread a tick for `thread_info`: with
100 threads, 5 ms a second of one core.

Symbolization (`object` over the executable, `dladdr` for shared libraries, demangling) runs
only when a profile or a stall is written, never while sampling.

## What 1% allows

A warm no-op build of 20 ms gives 200 µs: about 7,000 spans at 30 ns. A test build of 2 s gives
20 ms: about 700,000 spans. The span count is what to watch, because the sampler costs about
0.1% on either platform. What could exceed the budget is a hot loop wrapped in info spans, for
example a rust-analyzer `*_query` span per query execution in a front-end check: such a span
belongs at debug level, which `release_max_level_info` compiles out.

## How to measure it

1. The per-span and per-plot cost: in release, run
   `blaze test -p blaze-profile --release -- --ignored --nocapture what_a_span_and_a_plot_cost`.
   It prints the nanoseconds of a span and of a plot through the installed recorder.
2. The spans a build records: `blaze debug profile` after a warm no-op build and after a test
   build writes the flight recorder's trace; its `X` events (spans) and `C` events (plots)
   over the build's window give the counts. `blaze_profile::flight::progress()` before and
   after a build gives the span count directly.
3. The recorder's share: the cost a span times the spans over the build's wall time, on the
   critical path in the worst case (every span on the thread the build waits for).
4. The sampler's share: a warm no-op build and a test build timed with the daemon's sampler
   started and not (`blaze serve` starts it; the measurement build leaves out that one call),
   20 runs each, the difference of the medians.

Not measured yet: this lane does not build. The coordinator runs the four steps and records
the numbers here.
