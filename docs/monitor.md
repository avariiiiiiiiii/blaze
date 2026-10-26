# The monitor

The daemon keeps a [Flecs](https://www.flecs.dev/) world of what it saw happen: builds,
the patches its workers compiled, the live patches it sent to running programs, and the tests
`cargo ra-jit test` ran. It serves that world over Flecs REST on localhost. Agents query it,
and the Flecs explorer shows it. It is a mirror for watching, never an input: nothing reads
it back to decide a build.

## The decision

Andrew decided "do flecs as monitoring layer". These are the rules that follow from it, and
why.

- **Salsa stays the source of truth.** What a build is and what it reuses come from the
  workers' salsa databases and the store, as before. The Flecs world is told what happened
  after it happened, and nothing reads it back. A lost event leaves the world less complete
  and never makes a build wrong. With `RA_JIT_MONITOR=off`, every build is the same build.
- **Off the critical path.** A worker hands its event to a thread of its own and does not
  wait. The world runs on a thread of its own in the daemon. A client sends its build's event
  with one write to the daemon's socket after the build is over, and waits for no answer.
  Building the world (entities, relations, observers) is the daemon thread's work, done
  between builds. The cost is measured below.
- **Observers are the event stream.** Anything that has to react to a build, a patch or a
  test does it as a Flecs observer on the world: the `Totals` counters are the first.
  Observers run on the world's thread, so they never slow a build.
- **REST is the query API.** The Flecs REST server (`/query`, `/entity`, `/world`) is how
  agents and tools read the world. It is on by default on 127.0.0.1, because watching is the
  point: an agent that has to ask for the monitor first won't use it. It listens only on
  localhost, the daemon is per user already, and `RA_JIT_MONITOR=off` turns it off
  ([SECURITY.md](../SECURITY.md) says who can read it). Its schema is Flecs's own JSON, so
  any Flecs client works, the explorer too.
- **Future watchers are thin clients.** `blaze watch` ([#227](https://github.com/indexable-inc/blaze/issues/227),
  step 4), a TUI, or an LSP are clients of this REST API and its observers. None gets a
  channel of its own into the daemon.
- **The LSP is monitoring only.** The main editors are LLM agents that write whole files and
  then build. There is no keystroke-incremental `didChange` path to build for. An LSP would
  show what the monitor shows (diagnostics, patches, test results) and never drive a build.
- **Not the agent API.** [The agent API](roadmap.md#8-the-agent-api) (edit transactions,
  check, build, test and run on a root, in versioned CBOR over a socket,
  [#277](https://github.com/indexable-inc/blaze/issues/277)) is how an agent asks blaze to
  do something. The monitor is a separate, read-only view of what blaze did. The two never
  share state: the monitor is told after the fact, like everyone else.

## Turning it on and finding it

The daemon starts the monitor when it starts. It takes the first free port from 27870
(sixteen are tried), writes the address to `monitor` in its directory, and says where it
listens in its log and in `cargo ra-jit status`:

    $ cargo ra-jit status
    daemon: pid 1103090, version ..., exits after 1800s idle, workers hold at most ...
    monitor: Flecs REST on http://127.0.0.1:27870

`RA_JIT_MONITOR=off` turns it off; `RA_JIT_MONITOR=<port>` uses that port, or none if it is
taken. The daemon reads the variable when it starts. Workers and clients read it too, and
send nothing when it is `off`.

The [Flecs explorer](https://www.flecs.dev/explorer) connects to it as a UI: open the
explorer and set the remote to `http://127.0.0.1:27870`.

## What the world holds

Everything is in the module `blaze`. In REST answers a path's separator is `.` in the
`parent` field and `/` in an `/entity/...` URL.

| Entity | Component and tags | Relations |
|---|---|---|
| `workspaces.<name>-<hash>` | `Workspace` (root) | |
| `builds.b<n>` | `Build` (command, by, why, pid, at_us, took_us); `Succeeded` or `Failed`; `ByDaemon` or `ByCargo` | `(In, workspace)` |
| `programs.<name>-<hash>` | `Program` (path) | `(In, workspace)` |
| `patches.p<n>` | `Patch` (kind: `patch`, `object`, `unchanged`, `error` or `cargo`; functions, files, why, at_us, took_us); `Refused` when the worker did not build it | `(PatchOf, program)`, `(Of, build)` |
| `processes.<pid>` | `Process` (pid, executable) | `(Runs, program)` |
| `live.l<n>` | `LivePatch` (sequence, outcome `sent` or `restart`, why, functions, statics, at_us, took_us); `Sent` or `Restart` | `(PatchOf, program)`, `(PatchedInto, process)`, `(Of, patch)` |
| `crates.<package>` | | |
| `tests.<executable>` | `TestTarget` (package, kind, selection `affected` or `all`, total, ran, failed, status, at_us, took_us): the executable's last run | `(TestOf, crate)`, `(Of, build)` |
| `tests.<executable>.<module>.<test>` | `Test` (picked, why, path, outcome, ended_us); the last outcome as `Passed`, `Failed`, `Ignored` or `Unknown`; why it ran or not this time as `Changed`, `Skipped`, `Always`, `Sandboxed` or `Every` | `(TestOf, target)` |
| `blaze.Totals` | `Totals`: builds, by_daemon, by_cargo, errors, patches, objects, refused, live_sent, live_restarts, tests_passed, tests_failed, tests_skipped, since the daemon started | |

Some details:

- A patch is linked to its build when their times overlap (with 200 ms of slack), whichever
  is told first. A live patch is linked to the newest patch of its program.
- A test that was skipped keeps the outcome of the run before. Its `why` says why it was
  picked: for `Always`, the call path to what reaches outside its code.
- `ended_us` is the time from the start of the run to the test's `ok` line, so it is an
  upper bound on the test's own time when tests run in parallel.
- The world keeps the last 1000 builds, patches and live patches, and every test's last run.
- Numbers above 2^53 (the `*_us` times) come back as strings, as Flecs writes them.

## Queries an agent asks

The answers below are real, from `harness/monitor.py -v` and `harness/live_e2e.py` on Linux.
The query goes in `expr`, URL-encoded (`curl -G --data-urlencode`). `M` is the address
`cargo ra-jit status` prints.

**The patch an edit made, and the build it was for:**

    curl -sG $M/query --data-urlencode 'expr=blaze.Patch, (blaze.Of, $b)'

```json
{"results":[{"parent":"patches","name":"p6","vars":{"b":"builds.b7"},
  "fields":{"values":[{"kind":"patch","functions":1,"files":1,"why":"",
  "at_us":"1793031117464840","took_us":40000},0]}}]}
```

**That build:**

    curl -s $M/entity/builds/b7

```json
{"parent":"builds","name":"b7","tags":["blaze.Succeeded","blaze.ByDaemon"],
 "pairs":{"blaze.In":"workspaces.mon-ffb679"},
 "components":{"blaze.Build":{"command":"test","by":"daemon","why":"","pid":1103130,
 "at_us":"1793031117417929","took_us":48124}}}
```

**Which tests the edit reached** (`Changed`), **and which it did not** (`Skipped`):

    curl -sG $M/query --data-urlencode 'expr=blaze.Test, blaze.Changed'

```json
{"results":[{"parent":"tests.mon-1c99149b878911ee.tests","name":"uses_helper",
  "fields":{"values":[{"picked":"changed","why":"","path":"","outcome":"passed","ended_us":1656}]}}]}
```

    curl -sG $M/query --data-urlencode 'expr=blaze.Test, blaze.Skipped'

answers `plain` and `uses_other`.

**Which tests ran because they reach outside their code, and why:**

    curl -sG $M/query --data-urlencode 'expr=blaze.Test, blaze.Sandboxed'

answers `reads_a_file` with `"why":"it read `/dev/shm/.../mon/data.txt`, which changed"`.
With `RA_JIT_SANDBOX=off` the same test is `Always`:

    curl -sG $M/query --data-urlencode 'expr=blaze.Test, blaze.Always'

```json
{"name":"reads_a_file","fields":{"values":[{"picked":"always",
  "why":"it reaches `statx`, which reads or writes files",
  "path":"mon::tests::reads_a_file -> std::fs::read_to_string::<…> -> ... -> statx",
  "outcome":"passed", ...}]}}
```

**Which tests failed:**

    curl -sG $M/query --data-urlencode 'expr=blaze.Test, blaze.Failed'

answers `uses_other` after the harness makes it fail.

**The run of a test executable, and its build:**

    curl -sG $M/query --data-urlencode 'expr=blaze.TestTarget, (blaze.Of, $b)'

answers the `mon` library's tests with `"selection":"affected","total":4,"ran":1,"failed":0,"status":0`.

**The live patches sent to a running program, and the patch each came from:**

    curl -sG $M/query --data-urlencode 'expr=blaze.LivePatch, (blaze.PatchedInto, $p), (blaze.Of, $patch)'

```json
{"results":[{"parent":"live","name":"l5","vars":{"p":"processes.1340081","patch":"patches.p3"},
  "fields":{"values":[{"sequence":1,"outcome":"sent","why":"","functions":1,"statics":0,
  "took_us":3192, ...},0,0]}}, ...]}
```

**Which process runs which program:**

    curl -sG $M/query --data-urlencode 'expr=blaze.Process, (blaze.Runs, $prog)'

answers `processes.1340081` with `"prog":"programs.bin-284602"` and its executable's path.

**The counts since the daemon started:**

    curl -s $M/entity/blaze/Totals

```json
{"components":{"blaze.Totals":{"builds":4,"by_daemon":4,"by_cargo":0,"errors":0,
 "patches":3,"objects":0,"refused":0,"live_sent":0,"live_restarts":0,
 "tests_passed":5,"tests_failed":1,"tests_skipped":6}}}
```

Other useful forms: `blaze.Build, blaze.ByCargo` (builds the daemon left to cargo, with
`why`), `blaze.Patch, blaze.Refused` (what a worker declined), `blaze.LivePatch,
blaze.Restart` (live edits that needed a restart), `blaze.Test, (blaze.TestOf,
tests.<executable>)` (one executable's tests).

## Where it comes from

| What | Told by | Where |
|---|---|---|
| `Build` | the client, after its build ends (the command, who built it and why) | `cargo/mod.rs` `build`, `monitor::Build` |
| `Build` of an editor's edit | the daemon, when it relays it | `cargo/daemon.rs` `edit` |
| `Patch` | the worker, after each sync (from its answer: patch, object, refusal) | `main.rs` `Program::sync` |
| `LivePatch`, `Process` | the worker, when it sends a live patch or a restart | `live.rs` `publish`, `tell_restart` |
| `TestTarget`, `Test` | the client, after each test executable runs, with the selection from `affected.rs` | `cargo/run.rs` `tests` |

The events are the serde types in `crates/blaze-monitor/src/event.rs`. They go to the daemon
as a `Note` request on its socket, which it does not answer. The world, its observers and
its REST server are in `crates/blaze-monitor/src/world.rs`.

## What it costs

A one-function edit followed by `cargo ra-jit build`, with the monitor on and with
`RA_JIT_MONITOR=off`, each mode with its own daemon and copy of a small binary crate,
alternated in 100 pairs on dev-aws-8 (c7a.48xlarge, pinned to 16 cores, shared with other
work):

| | median | p10 | p90 | mean |
|---|---|---|---|---|
| monitor on | 40.0 ms | 38.9 | 40.7 | 39.9 |
| monitor off | 40.0 ms | 38.5 | 40.9 | 39.8 |
| on minus off, paired | +0.16 ms | -1.30 | +1.75 | +0.10 |

The difference is within the noise. A first version had the client wait at exit for a
thread to send its events, which cost +1.28 ms at the median; a client now writes its one
event to the socket itself.

## Checks

- `cargo test -p blaze-monitor`: sends one of each event to a world and checks the queries
  above over REST.
- `harness/monitor.py` (core, quick 2, Linux): a real daemon, an edit, and the patch and test
  queries an agent would ask ([harness.md](harness.md)).

## What's left

- The program's own answer to a live patch (applied, declined, why) from `ra-jit-live`: today
  the monitor shows what the worker sent.
- `blaze watch` as a client of the REST API and observers, with the watch mode of
  [#227](https://github.com/indexable-inc/blaze/issues/227).
- Builds cargo does wholly (no daemon running, or a command the daemon does not take)
  appear only when a daemon is there to be told.
- Per-test durations from the test harness itself, rather than the time to its `ok` line.
- macOS: `harness/monitor.py` runs on Linux only so far, and a process's executable path
  comes from `/proc`.
