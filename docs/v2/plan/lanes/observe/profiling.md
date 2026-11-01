---
status: plan
summary: draft of docs/profiling.md (lane observe): `nix run .#tracy`, the Tracy v0.14.1 viewer, beside a blaze built with `--features tracy`; then how to see where blaze's time goes with the always-on flight recorder and CPU sampler (`blaze debug profile`), the stall watchdog's reports, `BLAZE_PROFILE` and `BLAZE_STATS`, and Tracy behind the compile-time feature `tracy` (building, connecting, capturing headless, and a capture as text with tracy-csvexport). It moves to docs/profiling.md, status current, once the wiring it names lands.
date: 2026-10-31
---
# Profiling blaze

```sh
blaze build --release -p blaze-engine --features tracy   # a blaze that speaks to Tracy
nix run .#tracy                                          # the viewer, Tracy v0.14.1
```

Run the Tracy build of blaze (`target/release/blaze serve`, or any command), and it appears in
the viewer's list; click it. If the list stays empty, type `127.0.0.1` and press Connect.
`nix run .#tracy-capture` and `nix run .#tracy-csvexport` are the headless capture and the
capture as text (below). The three apps are built from Tracy's v0.14.1 source, pinned by hash,
because Tracy's protocol changes between releases and the client blaze bundles is v0.14.1; any
other viewer refuses to connect. They work on macOS and Linux (an X11 viewer, which a Wayland
desktop runs through XWayland). On a Linux that is not NixOS, the viewer needs the host's
OpenGL driver, for example through [nixGL](https://github.com/nix-community/nixGL).

That is one of five ways to see where blaze's time goes. Cheapest first, all in
`crates/blaze-profile`:

| what | cost | when |
|---|---|---|
| `BLAZE_STATS=1` | none until printed | a wall-time table of the build's steps, on stderr |
| the flight recorder and the CPU sampler | under 1% (see [speed-of-light](speed-of-light.md)) | always on; `blaze debug profile` writes the last minutes |
| the stall watchdog | one sum a loop turn | always on; writes a report when a request stops moving |
| `BLAZE_PROFILE=<dir>` | a few percent | every process's per-thread timeline, per-function and per-pass CPU time |
| Tracy (`--features tracy`) | a client thread, more per span | live timelines, plots, fibers, context switches, sampling, in Tracy's viewer |

## The flight recorder and `blaze debug profile`

Every blaze process keeps, per thread, a fixed ring of its last 32768 info-level spans (target
`blaze`, and rust-analyzer's `*_query` spans) and plots: memo hits and misses and joins per
build, the plane's queued and running tasks, the bytes of live frames and of all live memory.
Nothing allocates or does I/O per span. Next to it, a sampler records every running thread's
call stack 49 times a second: on Linux through `perf_event_open`, on macOS (aarch64) by
suspending each running thread for its registers and walking its frame pointers. Stacks are
named only when written.

```sh
blaze debug profile                 # the last 60 s, to state/profile-<time>.json
blaze debug profile --seconds 300   # a longer window, as far back as the rings reach
```

The file is a Chrome trace: open it in [Perfetto](https://ui.perfetto.dev) or
`chrome://tracing`. Spans are `X` events per thread, plots `C` counters, the end of each build an
instant event, and samples `X` slices on a `<thread> samples` track. Under its top-level
`blaze` key, `samples` holds the folded stacks (`a;b;c` and a count, most frequent first, the
input of a flame graph and readable as text) and `sampler` says whether sampling was on and,
when not, why (on Linux, usually `perf_event_paranoid`; `sysctl kernel.perf_event_paranoid=1`
allows it).

An agent asks for the same bytes with the API's `Profile { seconds }` request.

## The stall watchdog

The daemon counts progress: spans closed, query runs and tasks ended. When tasks are running
and none of these moved for the bound (60 s), it:

1. writes `state/stalls/<time>.json`, the flight recorder's trace of that moment with every
   thread's open spans;
2. fails each running request with the error `blaze stalled: no span closed and no task ended
   for N s while M tasks ran`, followed by each thread inside spans, outermost span first, and
   how long it has been in each;
3. stops the daemon, whose threads may be deadlocked; the next request starts a new one.

A deadlock (jobs that all wait on each other, as in blaze-pool's hang at 0% CPU) becomes this
error within the bound instead of a request that never ends.

## `BLAZE_PROFILE` and `BLAZE_STATS`

`BLAZE_PROFILE=<dir> blaze build` writes, for every process of the build, its spans with each
`blaze` span's own CPU time, merged at the end into `<dir>/trace.json` (Chrome trace),
`phases.tsv`, `functions.tsv` and `passes.tsv`. The flight recorder's plots and samples of the
build's window are in the trace too. `BLAZE_STATS=1` prints the build's wall time by step.

## Tracy

[Tracy](https://github.com/wolfpld/tracy) shows the same spans live, with plots, frames (one per
build request), fibers (one per plane task, so a task that moves between threads stays one row),
context switches and Tracy's own sampling. It is compiled in only with the feature `tracy`; a
default build has no Tracy code at all.

The client is the fork `indexable-inc/tracing-tracy` (tracing-tracy 0.12.0, tracy-client 0.19.0),
which bundles **Tracy v0.14.1**. Tracy's protocol changes between releases, so the viewer,
`tracy-capture` and `tracy-csvexport` must be v0.14.1 exactly. `nix/tracy.nix` builds them from
that release's source, pinned by hash, on top of nixpkgs' Tracy package (whose own version is
older). The flake's apps `tracy`, `tracy-capture` and `tracy-csvexport` and the dev shell's PATH
both use it.

### Build

```sh
blaze build --release -p blaze-engine --features tracy
```

The client starts on demand (`ondemand`): it records nothing and costs nothing beyond its thread
until a viewer connects, and listens on localhost only (`only-localhost`).

### Connect

Run the blaze built above (`blaze serve`, or any command; the daemon is the process doing the
work), then `nix run .#tracy` and pick the client from its list (it broadcasts to
`127.255.255.255`), or connect to `127.0.0.1`. Context switches and Tracy's sampling need
permissions: root on Linux, or `perf_event_paranoid` at most 1 for sampling.

The client listens on 127.0.0.1:8086 only. For a blaze running on another machine (an ix VM),
forward the port and connect the local viewer to `127.0.0.1`:
`ssh -N -L 8086:127.0.0.1:8086 <host>`.

### Capture headless

```sh
nix run .#tracy-capture -- -o build.tracy -a 127.0.0.1 -s 60   # 60 s, or until the client ends
```

Start it before the work. `build.tracy` opens later in `nix run .#tracy -- build.tracy`. In the
dev shell the same tools are on PATH as `tracy`, `tracy-capture` and `tracy-csvexport`.

### A capture as text

`nix run .#tracy-csvexport -- <flags> build.tracy`, or in the dev shell:

```sh
tracy-csvexport build.tracy                 # per zone: name, source, total, count, mean, min, max
tracy-csvexport -e build.tracy              # the same with self times (children left out)
tracy-csvexport -u build.tracy              # every zone's every run, with its thread
tracy-csvexport -f check -c build.tracy     # only zones whose name has "check", case-sensitive
tracy-csvexport -u -p build.tracy           # the plots' values (only with -u)
```

The CSV is what to diff between two builds or hand to an agent.
