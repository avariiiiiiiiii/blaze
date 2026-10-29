---
status: plan
summary: blaze v2: one sans-IO core under every build, one scheduler for every request (memo by input hash, single flight, fair share, critical path, memory admission), and the order the rest of the core moves onto it.
date: 2026-10-29
---
# blaze v2

The target is the design of the 2026-10-28 retrospective (sections "From scratch", "On the
biggest machines" and "For any Rust project"): the compiler a pure function of content hashes,
a thin platform-tuned IO layer around it, and one long-lived, always-warm service per project in
which every branch, pull request and CI job is a view. This note says what of it is built, where
each piece lives, and in what order the rest moves onto it. It builds on
[driver.md](driver.md) (whose steps 1 to 3 are on main) and takes the good ideas of
[#283](https://github.com/indexable-inc/blaze/pull/283) (blaze-next): its sans-IO core, its
Merkle VFS and journal, items by content hash, push invalidation and the agent API.

## The shape

    request (a view: a build, a test run, an edit)      many at once, from any client
        │  nodes, each keyed by the hash of its inputs
        ▼
    blaze-graph::Plane      sans IO: memo by key, single flight, fair share between
        │                   requests, critical path within one, memory admission
        │  Run { node, pool, near }
        ▼
    blaze-graph::exec       threads per core, grouped by last-level cache, stealing
        │                   from the nearest group first
        │  IO effects
        ▼
    blaze-core::Driver      io_uring (Linux), threads + clonefile (macOS), Sim (tests)

Nothing above `blaze-core`'s drivers does IO; nothing above the executor starts a thread; the
plane reads no clock. Each layer is tested by itself, the plane and the core under a seeded
simulation over many seeds.

## The plane

One plane per daemon, living as long as the daemon. A request adds nodes; a node has:

- a **key**: the hash of everything its value depends on ([unit-keys.md](unit-keys.md)'s rule),
  or none for work that is not a pure function of its inputs (running a test, linking to a
  path the user named);
- its **deps**, its **pool** (IO or a core), its expected **cost** (orders the ready nodes) and
  its expected **memory** (its predicted peak, in bytes).

What the plane does with them:

1. **Memo by key.** A node whose key was settled before, by any request, settles at once with
   that outcome (`Cached { remote: false }`), and runs nothing. The memo is the plane's, not a
   request's, so main and a pull request share every node whose inputs are equal.
2. **Single flight.** A node whose key is running or waiting for another request joins it: one
   task runs, and every request that asked settles when it does.
3. **Fair share.** Each request has a weight (an interactive edit more than a CI build). Of the
   requests with ready work, the next core goes to the one using the least of its share; an
   edit's work takes the next free core ahead of a background build's, and the build goes on
   after. Work under way is never stopped for it.
4. **Critical path.** Within a request, the ready node with the longest path of expected cost to
   the end of the request runs first; ties go to the node added first (driver.md).
5. **Memory admission.** The plane has a budget (`memory::Budget`, memory-budget.md). A node is
   started only when the predicted peaks of what runs, plus its own, fit; a node passed over for
   memory more than a few times holds back smaller ones behind it, so a large node is never
   starved. A node whose prediction alone is more than the budget fails before it starts, naming
   itself, its prediction and the budget: never swap, never a quiet retry with less at once.

The plane is a state machine (`Plane::step(input) -> outputs`) with the same inputs and outputs
as driver.md's machine, plus request ids; `Machine`, which the build-script and name-resolution
graphs use, is the plane with one request. Its tests drive random graphs of many requests under
`blaze-core`'s seeded random number generator, with random completion orders, failures and
memory, and check after every step: nothing runs before its deps; no key runs twice at once;
granted memory never exceeds the budget; every request ends; the same seed gives the same
outputs.

## The executor

Threads per core, grouped by the last-level cache they share (an EPYC's CCD, read from
`/sys/devices/system/cpu/cpu*/cache/index3/shared_cpu_list`; one group on a machine that says
nothing). The plane names, with each node it runs, the group its deps ran on last; the node is
queued there, and an idle thread takes from its own group first, then from the nearest other.
On macOS the groups are the performance and efficiency cores, and the critical path goes to the
performance ones.

## The IO core (`blaze-core`)

From blaze-next's `blaze-core`: effects with tags, completions, a `Driver` trait, and three
drivers: `Sim` (one thread, a virtual clock, shuffled completions, injected failures, torn
appends, power cuts), `Threads` (portable; `F_FULLFSYNC` on macOS) and `Uring` (Linux, its own
ring, no crate). v2 adds what the rest of blaze needs to move onto it:

| effect | Linux | macOS | Sim |
| --- | --- | --- | --- |
| read, read at, append, sync, truncate, timer | io_uring | threads | in memory |
| write a whole file atomically (temporary, sync, rename) | io_uring + `renameat` | threads | in memory, all or nothing over a crash |
| clone a file (a store value out to a target directory) | `FICLONE` | `clonefile` | shares the bytes |
| stat | `statx` | `fstatat` | in memory |
| watch a tree | fanotify, inotify where fanotify needs a privilege the process lacks | FSEvents | changes the test makes |

A clone where the file system cannot clone is an error that names the file system: no quiet
copy (no-fallback). Hard links are their own effect, which a caller chooses where it knows the
output is never written in place.

## What moves onto it, in order

Each step is a commit of this pull request, with its numbers in HANDOFF.md; the pull request
stays a draft until the gate and the rustc differential harnesses pass with the new path as the
only path.

1. `blaze-core` on main, with the new effects and their tests (simulation over many seeds,
   threads against io_uring on the same scripts).
2. The plane, its simulation tests, and `Machine` as its one-request case; the two graphs on
   main (build scripts and proc macros, name resolution) on it with a memory budget.
3. The executor's cache-aware groups.
4. Per-function codegen as plane nodes: each function's compile a node keyed by its code hash,
   so a function asked by two programs (or two requests) compiles once and the waves of
   `units.rs` become dependencies; the store write of a node overlaps the next compile
   (driver.md step 5).
5. Items by content hash in the front end (blaze-next's `blaze-items`, as a crate next to
   `hir-ty`): local, interface, code and deep hashes, cycles as Unison's. Invalidation and the
   store key on them: a body edit reaches its code hash and its callers' deep hashes only, a
   signature edit its dependents' interface hashes, a formatting edit nothing (places out of the
   code hash, patched at link time).
6. The store as the VM's disk: values uncompressed in mmapped packs (zstd only for the remote
   tier), outputs cloned out of it, no eviction on a pool VM; the front end's database as an
   mmapped image in the store, so a daemon starts by mapping it.
7. The workspace load, links and test runs as plane nodes (driver.md steps 4 and 6), and inputs
   committed while work runs (step 7).

## Rules that hold throughout

No rustc; no fallback (a node that cannot run fails and names itself); every key covers every
input, with a test that changes the input and sees the value rebuilt; build scripts and proc
macros sandboxed; semantics (item hashes, MIR) in the front end's crates, the plane, the
executor and the drivers outside it.
