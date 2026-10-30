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
