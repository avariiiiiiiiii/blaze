---
status: current
summary: The fuzzers: generated programs and edits, each held to blaze's own build from nothing (a warm daemon, a warm store), what a finding is, and what they do not cover.
---
# The fuzzers

Every other harness runs programs somebody wrote. The fuzzers make programs up (`fuzz/gen.py`)
and edits of them, and hold what blaze builds warm to what blaze builds of the same source from
nothing. None of them compares with rustc or cargo: the differential fuzzer that built each
program with cargo too (`fuzz/fuzz.py`) was deleted on 2026-10-30 with the other comparisons
([harness.md](harness.md)). Its account, with every finding, is in
[reports/FUZZ.md](reports/FUZZ.md), a record.

## Running them

    cd harness
    export BLAZE_BIN=$PWD/../target/release/blaze-engine
    uv run --frozen python fuzz/edits.py smoke '{"backends": ["cranelift"]}'   # fixed seeds; in CI
    uv run --frozen python fuzz/edits.py run '{"seed": "S", "rounds": N, "steps": K}'
    uv run --frozen python fuzz/edits.py run '{"seed": "S", "first": R, "rounds": 1}'   # round R again
    uv run --frozen python fuzz/edits.py check '{"files": ["FILE.rs"]}'    # stored sequences again
    uv run --frozen python fuzz/edits.py report '{"dir": "DIR"}'           # add up a run's logs
    python3 fuzz/store.py [--backends cranelift,llvm]                         # asm changes, cold and warm store; in CI

The fields each subcommand of `edits.py` takes are in its docstring. By default a program is
built under both backends, so `BLAZE_BIN` should be a build with `--features llvm`. LLVM is
run with `BLAZE_OPT=2 BLAZE_EMIT=fast`. The exit status is 1 when there is a finding.

## What a program is

`fuzz/gen.py` writes a program from a seed: `fuzz/prelude.rs` followed by one module per
case. A case is a set of types, traits and helper functions and a `run` function. Everything
a case computes is fed into one 64-bit checksum. `main` runs each case inside
`catch_unwind` and prints one line per case with its name, its checksum and whether it
ended or panicked, and the panic's message.

The source of case N of seed S depends only on S and N, so round N of a seed is the same
program whatever the number of jobs. The generator also combines scenes for async futures,
`dyn Any`, inline assembly and unwinding (`fuzz/scenes.py`), and a few `const fn`s per case
(`fuzz/constfn.py`), each called once as a constant and once through `black_box`, both into
the checksum, so blaze's const evaluator and its compiled code have to agree. The programs
avoid what two correct builds may print differently: arithmetic is wrapping, checked,
saturating or overflowing; no address, NaN bit pattern, sign of a zero or hash order is
observed; every loop is bounded. They are a single crate and use only `std`.

## What counts as a finding

`fuzz/edits.py`: a round is one program under a daemon that keeps its binary up to date. Each
step edits the source (a case's body, functions, constants or types regenerated; a constant's
value; fields or variants reordered; `#[repr(C)]` put on or taken off; one literal; a function
added, its signature changed or removed; an earlier step undone) and asks the daemon for the
binary, which it splices, patches or links in full. The same source is built by blaze from
nothing, with no daemon and no store (`fuzz/runs.py`'s `Project`). A finding is a step whose
binary prints otherwise than the one from nothing, a step the daemon refused, a daemon that
died, or one that panicked and went on. A finding's sequence is kept under `fuzz/found/edits/`
for `check` to run again.

`fuzz/store.py`: one function compiled, then only the text of its `asm!` changed, each version
read back from the store; and the smoke programs built with no store, into a fresh store and
from the store that filled. Every build with a store has to run as the one without, and leave
no function out. This covers #418, where two `asm!` wrappers with the same Cranelift IR and
different assembly shared one entry of LLVM's store (the assembly is in the key since
`2a3a0a35`).

## From a finding to a fix

1. `edits.py run` with the round's seed and `first` builds it again.
2. Reduce it to a test ([harness.md](harness.md#adding-a-test)): a step of the daemon harness
   of that area whose warm build has to give what blaze's build from nothing gives.
3. Fix it, run `edits.py check` and the gate.

## What they have found

The differential fuzzer found twenty-one problems in whole builds before it was deleted, most
of them miscompiles of drop order; [reports/FUZZ.md](reports/FUZZ.md) lists them and
[results.md](results.md) tabulates the runs. Generated edits against a running daemon found
four more problems in the incremental path, all fixed, and a fifth that is open: after some
edits a panic inside `core` names the file by its `/rustc/<commit>/library/...` path instead of
the local one.

## What they do not cover

- A program blaze builds wrong the same way warm and from nothing: since 2026-10-30 that is for
  real crates' own test suites to find, not a fuzzer.
- Generated edits have had a few short runs only, so the daemon's incremental path, the patch
  object and the splice are barely fuzzed.
- Only Linux x86-64 has been run; the smoke run is in the Linux x86-64 CI jobs only.
- Single-crate programs using only `std`: nothing about dependencies, features, build
  scripts or proc macros. No threads, macros of their own, SIMD intrinsics, or I/O other than
  the checksum lines.
