---
status: plan
summary: lane usercode's speed of light: the bound each component's work and the hardware set, the arithmetic, and how the design reaches 90% of it.
date: 2026-10-29
---
# Lane usercode: speed of light

The bar is "Within 10% of the theoretical maximum, everywhere" in
[principles.md](../../goals/principles.md). Each component of lane usercode has a section here,
written by the agent that owns it. Unless a section says otherwise, figures are for one core of
the target machines (EPYC Turin, Graviton 5, [hardware.md](../../design/hardware.md)):
- about 3.5 GHz, so figures in ns use 3.5 GHz;
- streaming reads from L1 or L2 at about 32 bytes a cycle, which is also memchr's rate;
- UTF-8 validation of mostly ASCII text at about 16 bytes a cycle (std's word-at-a-time path);
- about 2 instructions per cycle for branchy code and about 4 for straight-line integer code;
- a small allocation from a thread cache in about 20 cycles;
- a system call that resolves a path (stat, openat, mkdir), with the path cached, in about
  0.4 µs, and one on an open descriptor (write, fstat, futimens, close) in about 0.15 µs.

None of this has been measured yet: nothing compiles until every lane is done. `proof`'s budgets
measure each bound once it does, and a component more than 10% above its bound is a bug.

## Build scripts: `crates/blaze-build-script/` (usercode-fast-scripts)

What each part does and the state of the art it is compared with:
[scripts.md](scripts.md). Workloads, from scripts as published: a typical script prints about
20 lines (1 KB); a heavy one (cc, bindgen) about 500 lines (40 KB), of which about 400 are
directives kept. A unit's environment is about 100 variables cargo sets (40 fixed, 20
features, 30 cfgs, 10 from dependencies) and about 60 inherited, so about 150 pairs and 8 KB.
A root `build.rs` is 0.3 to 5 KB.

### What a script prints (`output::read`, `parse`, `watch::watched_in`)

Work: split \(n\) bytes into \(L\) lines, validate the \(n_d\) bytes of directive lines as
UTF-8, and dispatch each directive.
- **Bytes.** Line breaks at \(n/32\) cycles, validation at \(n_d/16\).
- **Instructions.** Per line, the minimum is about 8 to find the first byte that is not
  space and test `cargo:`, about 10 for the key (its length, then its bytes), about 5 to find
  `=`, and about 10 to hand the directive on: about 35, at IPC 2 about **18 cycles a line**.
- **Allocations.** `parse` hands back an `Output` of owned strings: one allocation of 20
  cycles per kept value \(D\).
- **The bound** for `read` is \(n/32 + n_d/16 + 18L\) cycles. For the heavy script: 1,250 +
  2,500 + 9,000 = **12,750 cycles, 3.6 µs**. `parse` adds \(20D\) = 8,000: **20,750 cycles,
  5.9 µs**. A typical script is about 0.15 µs.
- **Where the design is.** At the bound: one pass, each line looked at once, no copy but a
  kept value, and no search of values unless `OUT_DIR` moved (then one `memmem::Finder` built
  per output).
- **What stands in the way** of `parse` reaching `read`'s bound is the 400 allocations, which
  the `Output` type's owned fields make part of the work. A caller that needs only part of
  the output reads it with its own `Sink` and copies nothing else (`watched_in` does). An
  `Output` held as one buffer with spans would remove them; that changes every caller of
  `Output`'s fields in blaze-cargo and blaze-usercode, so it is asked for in
  `v2/requests/usercode-fast-scripts.md` (request 7), not done here.

### The environment (`env`)

Work: make about 100 names and values, order them, and merge them with about 60 inherited.
- **Allocations.** `Env::vars` holds owned pairs: for about 150 pairs that is up to 300
  allocations, about **6,000 cycles**, and the copy of 8 KB, about 250.
- **Values.** Each derived value is made at its length without `format!` (`envify`,
  `prefixed`, `decimal`): about 20 cycles plus a cycle per 16 bytes, for 100 values about
  **2,500 cycles**.
- **Order.** What cargo sets comes in about five groups that could each be sorted already;
  their merge is \(100 \log_2 5 \approx 230\) comparisons. A comparison of two names by a
  `u128` of their first 16 bytes is about 4 cycles (a tie goes to the bytes), so about **900
  cycles**.
- **Inherited.** The inherited environment is the same for every script of a build. Passed
  sorted, checking that is 60 comparisons of names (about 10 cycles each), **600 cycles**, and
  the merge of about 160 entries about **800 cycles**.
- **The bound** is about 6,250 + 2,500 + 900 + 600 + 800 = **11,050 cycles, 3.2 µs** a unit.
- **Where the design is.** The same allocations, values, check and merge, but what cargo sets
  is sorted whole: \(100 \log_2 100 \approx 660\) comparisons, about 2,600 cycles in place of
  900. That is about **12,750 cycles, 15% over the bound**. Before this pass, the sort key was
  the first 8 bytes, which tie on most names (`CARGO_CF`, `CARGO_FE`, `CARGO_PK`) and cost
  about 10 cycles a comparison (6,600 cycles, 37% over); and the inherited environment was
  sorted with the rest (another \(60 \log_2 60 \approx 360\) comparisons, 3,600 cycles). Both
  are removed: the key is 16 bytes, and an inherited environment passed sorted is only checked.
- **What stands in the way.**
  1. The whole sort of what cargo sets, about 1,700 cycles. To remove it, each group is made
     in order (the fixed names from a table sorted at compile time, features in the
     manifest's sorted order, cfgs sorted once per target) and the groups are merged. This
     is the next change here, once `proof` confirms the 15%, since the estimate is close to
     its own error.
  2. Callers that pass the inherited environment unsorted pay the 3,600 cycles back. Request
     6 asks blaze-usercode and blaze-cargo to sort it once per build.
  3. The 6,000 cycles of allocations, more than half the bound, are the owned `Env::vars`.
     An `Env` held as one buffer with spans would remove them. Callers move the pairs into a
     run's key and a `Command`, so this is asked for with request 7.

### Scripts read rather than run (`elide`, `elide::lex`)

- **Refused at the start** (most scripts, which start with `use`, `#!` or an attribute). The
  work is to find the root file among the script's files (a `BTreeMap` lookup of 2 to 4 path
  comparisons, about 100 cycles) and look at its first bytes (about 10). **The bound is about
  110 cycles, 30 ns.** The design is at it: `may_open_main` refuses on the first byte that is
  not white space when it is neither `/` nor `f`. Before this, the whole file was validated,
  searched three times (`\r\n`, a lone `\r`, text direction code points) and its first token
  lexed: for 3 KB about \(3000/16 + 3 \cdot 3000/32 \approx 470\) cycles more, 5 times the
  bound.
- **Read to the end** (a script elision may accept), \(n\) bytes and about \(T = n/6\)
  tokens:
  - **Bound.** One classification of every byte at about 0.2 cycles a byte (simdjson's stage
    1: 64 bytes a step into masks) and about 10 instructions per token (5 cycles) give
    \(0.2n + 5n/6 \approx\) **\(1.0n\) cycles**. For a 300-byte accepted script that is about
    300 cycles.
  - **Where the design is.** Validation \(n/16\), three SIMD searches \(3n/32\), a table load
    per byte outside literals (about \(0.6n\)), and about 30 instructions per token (15 cycles,
    \(2.5n\)): about **\(3.3n\) cycles, 3.3 times the bound**. For 300 bytes, about 1,000
    cycles (0.3 µs).
  - **What stands in the way** is the per-token instruction count and the per-byte loop. The
    fix is a stage 1 over `std::simd` (safe code, `portable_simd` on the pinned nightly): 64
    bytes a step into masks of identifier bytes, spaces, quotes, `\r` and `0xE2` (the text
    direction check folded in), with token boundaries from the masks' bits. The lexer then
    does about 10 instructions a token. This is owned here and comes first after `proof`
    measures, since every accepted script is small and read once per edit of its source.
- **Printing** what an accepted script prints copies at most \(16n\) bytes (the bound against
  scripts that print the square of their size), at 32 bytes a cycle.

### What reruns a script (`watch`)

Work per printed path: join it to the package's directory (about 70 bytes), keep it once, and
copy it out if it is new.
- **Bound.** Copying 70 bytes is about 2 cycles and the allocation 20. Keeping each once must
  use `Path` equality, as cargo's does (`a//b` and `a/./b` are `a/b`), and a hash consistent
  with it reads each byte at a component boundary check, about 1 cycle a byte, so 70. **About
  92 cycles a path.**
- **Where the design is.** About 100 cycles: one reused join buffer, one copy per kept path, a
  scan up to 16 paths and std's `Path` hash past that, within 10% of the bound.

### Rerun checks and directory times (`apply::Rerun::stale`, `newest`)

This is I/O bound, and the bound is the number of system calls.
- **Bound.** One stat per watched file. For a watched directory tree of \(D\) directories,
  \(E\) entries and \(K\) links: \(D\) times (openat, getdents64 per about 200 entries, close),
  plus one fstatat per entry, plus one stat per link's target. For `src` with 50 directories
  and 500 files that is \(50 \cdot 3 + 500 = 650\) calls: about 150 on descriptors and 500
  relative to one, **about 120 µs**.
- **Where the design is.** At the bound. Each entry's time is read relative to its directory
  (`DirEntry::metadata`), a path is made only for a link or a directory, and a directory is
  visited once by device and inode from the metadata already read. Before this pass, a
  directory was checked by `canonicalize` (glibc's `realpath`: about one call per component,
  so about 8 per directory, 400 here), and each entry was stat'd by its full path: about 1.6
  times the bound.
- **Beyond the bound of these calls**, batching the stats through io_uring (`IORING_OP_STATX`)
  would cut the per-call cost. That is the daemon's effects (lane core), not this crate's
  std calls; request 2 moves runs' files to effects, and `stale` can follow it.
- **Allocations.** A check that finds nothing stale allocates nothing for files and
  variables: the path is built in a buffer the caller keeps across checks, and the lookups
  return borrowed values (`a_fresh_run_is_checked_without_allocating` holds it to zero). A
  watched directory still allocates what std's `read_dir` does, its handle and a copy of its
  path, per directory; reading directories without allocating needs `getdents64` into a
  frame buffer, which is the daemon's effects (lane core), not this crate's safe std calls.

### Starting and finishing a run (`run::begin`, `Ran::finish`, `run::judge`)

- **`begin`**, the directories existing (every run but the first): `mkdir` of `OUT_DIR`,
  which fails as existing, and std's check that it is a directory (2 calls); in cargo's
  second layout the same for the run's directory (2); then openat, write, fstat and close of
  `invoked.timestamp` (4). **The bound is 6 calls (8 in the second layout), about 1.6 µs.**
  The design is at it. Before this pass, the first layout made the run's directory again
  (it holds `OUT_DIR`, so it already existed: 2 more calls) and stat'd the timestamp by path
  after closing it.
- **Writing cargo's files**: the printed file is openat, write, futimens and close (4), then
  `stderr` and `root-output` are 3 each. **The bound is 10 calls, about 2.3 µs.** The design
  is at it. Before, the printed file was written, closed and opened again to set its time
  (12 calls).
- **`judge`** is `logged`'s one search for `\ncargo:` (\(n/32\)) plus `parse`'s bound above.
- **The run itself** is bound by spawning the script, its own work and waiting for it:
  posix_spawn and exec of a dynamically linked binary take about 100 to 300 µs. Everything
  this crate does around a run is about 14 calls, under 5 µs, so a run's critical path is the
  script. The wait on `ETXTBSY` (a script another thread's fork still holds open) only
  happens when that fork races the write of the script; spawning from one place in the
  daemon (lane core) removes it.

### Who receives what (`apply::to_link`, `Compile`)

- **`to_link`** walks the unit graph once: the bound is one node made per edge and one copy
  per script run received. The design is at it. It made each dependency's node twice before,
  and followed dependencies that receive nothing.
- **`Compile::args`** borrows each argument but a path read lossily and a `link-arg=`,
  which it makes at their exact length.
- **`to_link_in`** lends the memo's entry: a unit met again allocates nothing
  (`a_unit_met_again_is_free`).

### Memory by lifetime (docs/v2/plan/design/memory.md)

What this crate allocates, by how long it lives, and the allocator each belongs on:

| what | lives | allocator |
|---|---|---|
| `Output`, `Env`, `Compile`, `Watched`: owned results | a kept run, a unit's build | the revision arena (request 7: one buffer with `u32` spans) |
| elision's printed text and argument list | one read of a root file | the frame allocator, once callers hand one in |
| `Rerun::stale`'s path | across checks | the caller's per-worker scratch |
| messages, errors | when one is shown | the general-purpose allocator: the edge |

No crate uses `blaze-mem` yet, and this crate's results are owned types its callers keep, so
moving them onto an arena is a change of their shape with the callers (request 7). The paths
that run in the warm loop already make no general-purpose allocation. Those are reading what
was printed, lexing, a refused root file, the check that a kept run is fresh, a memo hit,
environment lookups and the check of a run for errors, and the allocation-budget tests hold
each to zero.

### What `proof` measures

Each figure above, with its bound times 1.1 as the budget:
- cycles per byte and per line of `read` and `parse` on the corpus of `harness/scripts.py`;
- nanoseconds per unit of `env`, with the inherited environment sorted and not;
- nanoseconds per refused root file, and cycles per byte of a root file read to the end;
- system calls (strace's count) of `begin`, `Ran::finish` and `newest` over a fixed tree.
