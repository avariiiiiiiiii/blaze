---
status: plan
summary: blaze v2: the bigger changes for the fastest compile times: tiered compilation and idle cores always working, lazy per-function compilation, copy-and-patch codegen, no binary on disk for dev runs, one global cache, semantic edits and declared intent, shared generic code, exact test selection, hash-consing, premapped dependencies, scheduling from history, PGO and BOLT.
date: 2026-10-29
---
# The fastest compile times

[model.md](model.md) and the lanes make every step incremental. These changes go further: they
remove steps entirely, or make the ones left an order of magnitude cheaper. Lane `fastcompile`
builds them ([building.md](../process/building.md) section 18), asking the owning lanes for
hooks.

## Compile less

1. **Lazy compilation, per function.** A dev run or test compiles only the functions that
   execute. Every other function is a stub that asks the daemon for its code on first call,
   gets it patched in, and jumps. Most of a binary never runs in one test. The live runtime
   (`blaze-live`, lane link) already patches functions into a running process; this makes that
   the normal path for dev and test runs. Release builds and `blaze build` outputs are compiled
   whole, as rustc does.
2. **Shared code across generic instances.** Instances whose type arguments have the same
   layout and the same trait-method resolutions share one compiled body. Rust doesn't
   guarantee distinct function addresses for them. Statics, `TypeId`, `type_name` and anything
   else whose result the program can observe as different stay per instance. MIR-level
   analysis decides which instances can share (lane db, codegen).
3. **Exact test selection.** Each test run records the code hashes of the functions it
   executed. After an edit, only tests that executed a changed function, or read a changed
   input, run again. The rest report their cached result, marked as cached (lane usercode).

## Compile cheaper

4. **Copy-and-patch codegen for dev builds.** At blaze's build time, LLVM compiles one stencil
   per MIR operation and operand shape, with holes for constants, offsets and jump targets.
   Compiling a function is then copying stencils and patching holes, roughly 10 to 100 times
   faster than cranelift. The code is close to cranelift's for straight-line code. Hot
   functions, found by the live runtime's counters, are recompiled with cranelift or LLVM and
   patched in. Its output is checked against cranelift's on the corpus, byte for byte in
   behavior.
5. **Hash-consing of query values.** Equal values are stored once, so comparing two is
   comparing IDs, and a value's fingerprint is computed once when it is interned. Early cutoff
   no longer hashes whole values (lane db, format).

## Don't do it at all

6. **No binary on disk for dev runs.** The daemon loads code straight into the process it
   runs. Code pages are mapped twice from one memory object, writable in the daemon and
   executable in the process, so a patch is a store with no `mprotect` and no link. Fences make
   the change safe for running threads: the old function stays valid until no thread is in it
   (plane task boundaries, [concurrency.md](concurrency.md)). The executable is written only
   when a build asks for one.
7. **Dependencies as one premapped image.** Everything below the workspace members is
   prelinked once into one shared image, keyed by the dependency graph's hash and mapped by
   every dev run, so a dev link covers only the workspace's own code.
8. **One global cache.** Results that depend only on public inputs are computed once for
   everyone and fetched instead of built:
   - crates.io crate versions, std, sandboxed proc-macro outputs and build-script outputs;
   - LLVM-optimized functions, keyed by MIR hash, target and settings.

   The remote tier verifies every fetched value by its content hash and serves a cold build of
   the dependencies as a download (lanes store, ci).

## Let the users help

9. **Semantic edits.** The agent API takes edits by meaning: replace the body of `f`, add a
   field to `S`, rename `x`. Each edit is applied to the flat syntax directly and names exactly
   what changed. There is no diff and no reparse, and invalidation starts from the named item.
   Text edits keep working.
10. **Declared intent.** An agent can say what comes next: "I will run test T" or "here is the
    edit I am about to save". The daemon compiles only what T needs, starts speculatively on
    the announced edit at low priority, and drops that work if the edit doesn't come.

## Tier up, and never sit idle

13. **Tiered compilation for dev and test runs**:
    - **Tier 0, copy-and-patch.** Every function starts here, on first call (changes 1 and 4),
      with counters on calls and loop back-edges.
    - **Tier 1, cranelift.** Warm functions are recompiled in the background and patched in
      (change 6).
    - **Tier 2, LLVM at full optimization**, using the profile the counters gathered (branch
      weights, hot call sites), for hot functions.
    - **On-stack replacement at loop headers**, so a long-running test moves to faster code
      partway through.

    No user picks a back end; the profile does, per function. Tiers change only optimization.
    Overflow checks, debug assertions and `cfg` come from the chosen profile as under cargo, so
    a correct program behaves the same at every tier.
14. **Idle cores always work.** With no foreground request, the daemon spends idle cores, at
    the lowest OS priority (`SCHED_IDLE`, background QoS on macOS) and under memory admission,
    on:
    - higher tiers for functions past runs showed hot, hottest first, stored by code hash and
      profile, so the next run starts with them;
    - the release build of what changed, so `blaze build --release` is usually already done;
    - the dependency image (change 7) and global-cache fetches (change 8) for branches likely to
      be built.

    A foreground request preempts all of it at once. `blaze build` outputs stay whole,
    ahead-of-time and byte-identical; tiering applies only to processes the daemon runs.

## Make blaze itself faster

11. **Scheduling from history.** Each task's past duration and peak memory, by query kind and
    key, feed the plane's critical-path ordering and memory admission, which today use
    estimates.
12. **blaze built with PGO and BOLT** on profiles of its own daemon doing real builds
    (`tools/pgo`), every release.
