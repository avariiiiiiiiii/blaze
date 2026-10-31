---
status: plan
summary: lane compile-time-sched: how v2 compiles build scripts and proc macros in the job's one database and plane, and the single flight that makes a function they share compiled once between them.
date: 2026-10-30
---
# compile-time-sched: build scripts and proc macros in one scheduler, one database

## What was slow

v1's `crates/blaze-cargo/src/cargo/compile_time.rs` ran each build script and proc macro as
its own `blaze-engine` process (`cargo/own_script.rs`) over its own `rust-project.json` and
database, at most `ceil(cores/4).max(4)` at once: 5 on 18 cores. std was loaded and its names
resolved once per process (41 times in the cold lane's build of blaze), and `syn`, `quote` and
`proc-macro2` analysed and compiled once per proc macro.

## Where v2 already stands

- **One database.** `blaze_lower::lower` lowers every unit of a job, build scripts and proc
  macros included, into the job's one `base_db` database: std, `syn` and the rest are loaded,
  resolved and checked once a job, by salsa.
- **One scheduler.** Every unit is a node of the job's plane (`serve/job.rs`): a compile is a
  `Task::Thread` of `kinds::COMPILE` on the work pool, a build script's run a `Task::Loop` of
  `loops::SCRIPT_RUN`, the sandboxed driver's spawn, which holds no worker while it waits on
  its child or the C compiles it starts. A proc macro is loaded in the daemon with
  `blaze_macro_eval::load`, isolated by construction.
- **Sandboxed runs, unchanged.** `serve/unit/script.rs` runs every build script under the
  sandboxed driver as before; this lane changed nothing of how a run is made.
- **The per-process path is deleted.** `compile_time.rs`, `own_script.rs` and `own_macro.rs`
  were called by v1's engine only, deleted on 2026-10-30, and are gone with the rest of
  `blaze-cargo/src/cargo/` (lane integrate-load's step 4). Names of them left in prose:
  `harness/wasmcmacro.py`'s docstring and a reason string in `blaze-cargo/src/toolchain/own.rs`.

## What was left: a shared function compiled once per unit

A registry crate a build script or proc macro depends on (`syn`, `cc`) is `reached_only`: its
own unit compiles nothing, and each program's walk compiles what it reaches. The codegen
driver's memo (`Memo`, one per daemon, `driver::shared_memo`) shares templates by shape, but a
build fills it only as its compiles end, and the store only once the unit ends. Three proc
macros' walks started together all missed both, so each prepared and compiled every `syn`
function it reached: three compiles of the same function.

The fix is a single flight, `blaze_codegen::flight`, one table per memo:

- After the store misses, a build **claims** each function it would compile, by the key the
  store keeps its template under (dev) or its blob key (release). The first to ask owns it;
  the others are told another build is compiling it, or that it landed and under which shape.
- The owner compiles what it owns, then **lands** each (the memo has its template under the
  shape the landing names) and **lets go** of the rest (a prepare or compile that failed, a
  stop), before it waits on any other build's claim.
- A build then **waits** for the claims of the others, and makes each landed function from
  the memo's template with its own places, neither prepared nor compiled: `Stats::flown`. One
  let go of is claimed again; one of a failed compile is compiled by the next to ask, which
  reports its own error.

No build holds a claim while it waits, so builds waiting on each other's claims cannot
deadlock: each claim belongs to a build that is compiling, which lands or lets go of it. The
wait is a condition variable, checked for a stop every 5 ms. The table is the memo's, so a
test's fresh memo has a fresh table, and `Memo::retain` drops each landing with its template.

What each build puts in the store is what it would put alone: a landed function's template is
put under the build's own stored key, as its own compile's would be. The flight keeps no key
of its own; the store's keys (lane warm-store) are untouched.

## Tests

`crates/blaze-codegen/src/driver/tests/flight.rs`: three proc macros' expanders reaching
`syn`, `proc-macro2` and `quote`, and two build scripts' `main`s reaching `cc` and `libc`, built
at once on one memo with compiles slowed so they overlap. Each shared function is prepared and
compiled once between them, counted by key at the backend; the builds' `compiled` and `flown`
add up to the functions each reaches; each build's functions and puts are those it makes
alone. The same for release compiles by blob key. `crates/blaze-codegen/src/flight.rs` tests
the table: first asker owns, a claim let go of is the next asker's, a waiter wakes for the
landing and a stopped one gives up, builds waiting on each other both end.

The brief asked the count to come from the events; v2's daemon writes no per-function events
(v1's `Event::Functions` was its build page), so the test counts at the backend and in each
build's `Stats`.

## Program builds admitted by the memory budget (queue 01101, 2026-10-31)

Main's #1539 and #1568 (the deleted `blaze-engine/src/admit.rs`): a worker's program builds
share its memory budget, and one not yet measured runs alone and is measured. v2 had the
plane's admission but not that rule. Every unit's compile was a `kinds::COMPILE` node with no
key, so all of them were predicted by one per-kind estimate, 64 MiB before any was seen. That
estimate came from `arena::measure`, which counts only the task's own thread, while a
program's codegen runs on rayon's threads and its link on blaze-link's. On a cold runner,
therefore, as many program builds as there are work threads could start together, each
predicted far under what it holds: v1's failure in #1550 (14.5 GB resident, 23 GB swapped).
The process-wide `held` gate stops only new starts, not builds already under way growing
together.

Now, as docs/memory-budget.md's plan has it:

- **One kind and one key per program.** A unit that builds a program its client runs (a
  binary, test, bench, example or `cdylib`; `serve::unit::program_key`) is a
  `kinds::PROGRAM` node, pushed with `Plane::push_program` and its program's key. The key is
  the package, target, profile, platform, target kinds and mode. The history learns by that
  key; the plane's node has none, so nothing is single-flighted or memoized by it. Build
  scripts and proc macros stay `COMPILE`: many and small, and v1 did not admit them this way
  either.
- **Never measured: alone.** With a budget, a program the history has no record of (or one
  measured to hold all of it) is predicted the whole budget. The plane (`pick`) starts a task
  predicted the whole budget once no task granted memory runs, whatever the process holds
  besides, and while it runs nothing granted memory fits.
- **Measured alone, across every thread.** As it starts, `arena::high_water_from_now` begins
  a watch of the process's high-water mark. That is a `fetch_max` in the allocator's cold
  flush path, once per 256 KiB of a thread's drift, so a watch costs an allocation nothing.
  When the task ends, it learns the rise.
- **Measured beside others, never lowered.** Its thread's own peak misses its compiles, so it
  learns at least its prediction, and a record made alone is not worn down by runs that
  cannot see it.

Tests: `plane_tests::a_task_predicted_the_whole_budget_runs_alone`,
`arena::tests::a_watch_sees_what_every_thread_held_at_most` and
`serve::plane::tests::a_program_is_admitted_by_its_own_measure_and_one_never_measured_runs_alone`.

What is still open: a task's peak beside others is still only its own thread's
(`arena::measure`). Attributing to a task what the threads it hands work to hold would take
a scope that rayon jobs enter, at blaze-codegen's `until_stopped` and blaze-link's parallel
sites; that is lane core's allocator. Until then a program's record changes only when it
runs alone again (after the record is evicted) or holds more than predicted on its own thread.

## Left open

- **Release summaries.** A release build still prepares and summarizes every function the store
  has no summary of before it claims compiles: two release builds of one `syn` both summarize
  it. Claiming summaries by stored key is the next step.
- **Folded groups** (`fold`) and **lazy builds** (`Build::run_later`, `Build::compile_one`)
  do not claim.
- **Keys split by policy.** A unit's settings hash the whole `Policy` id, so two proc macros'
  `syn` share a key only when their policies are equal; per-package profile overrides that
  differ between their closures give each its own key and compile. For the codegen lane.
- **The memo only grows.** Nothing in the daemon calls `Memo::retain`; release templates now
  also live in it, under their blob keys.
